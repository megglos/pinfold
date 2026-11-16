//! Frame-time captures: reading PresentMon CSV files and turning frame
//! times into the usual statistics. Pure functions, no OS access.

/// Frame times in milliseconds from a PresentMon CSV.
///
/// The frame time column is `MsBetweenPresents` (default metrics) or
/// `FrameTime` (2.x metrics). When the capture holds several processes,
/// the one with the most frames is taken: games often have helper
/// processes with the same name that present nothing.
pub fn parse_csv(text: &str) -> Result<Vec<f64>, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("the capture is empty")?;
    let cols: Vec<String> =
        header.trim_start_matches('\u{feff}').split(',').map(|c| c.trim().to_lowercase()).collect();
    let time_col = cols
        .iter()
        .position(|c| c == "frametime" || c == "msbetweenpresents")
        .ok_or("the capture has no FrameTime or MsBetweenPresents column")?;
    let pid_col = cols.iter().position(|c| c == "processid");

    // (pid, frame times), in order of first appearance.
    let mut groups: Vec<(String, Vec<f64>)> = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        let Some(value) = fields.get(time_col).and_then(|v| v.trim().parse::<f64>().ok()) else {
            continue;
        };
        if !value.is_finite() || value <= 0.0 {
            continue;
        }
        let pid = pid_col.and_then(|i| fields.get(i)).map(|p| p.trim()).unwrap_or("");
        match groups.iter_mut().find(|(p, _)| p == pid) {
            Some((_, times)) => times.push(value),
            None => groups.push((pid.to_string(), vec![value])),
        }
    }
    groups
        .into_iter()
        .map(|(_, times)| times)
        .max_by_key(|times| times.len())
        .ok_or_else(|| "the capture contains no frames".to_string())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub frames: usize,
    pub seconds: f64,
    pub avg_fps: f64,
    /// Average FPS over the slowest 1 % of frames.
    pub low1_fps: f64,
    /// Average FPS over the slowest 0.1 % of frames.
    pub low01_fps: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub max_ms: f64,
}

/// Percentile with linear interpolation between closest ranks.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
}

/// Average of the slowest `share` of the frames (at least one frame).
fn slowest_avg(sorted: &[f64], share: f64) -> f64 {
    let n = ((sorted.len() as f64 * share).ceil() as usize).clamp(1, sorted.len());
    sorted[sorted.len() - n..].iter().sum::<f64>() / n as f64
}

pub fn stats(frame_times_ms: &[f64]) -> Option<Stats> {
    if frame_times_ms.is_empty() {
        return None;
    }
    let mut sorted = frame_times_ms.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let sum: f64 = sorted.iter().sum();
    let n = sorted.len();
    Some(Stats {
        frames: n,
        seconds: sum / 1000.0,
        avg_fps: 1000.0 * n as f64 / sum,
        low1_fps: 1000.0 / slowest_avg(&sorted, 0.01),
        low01_fps: 1000.0 / slowest_avg(&sorted, 0.001),
        avg_ms: sum / n as f64,
        p50_ms: percentile(&sorted, 50.0),
        p95_ms: percentile(&sorted, 95.0),
        p99_ms: percentile(&sorted, 99.0),
        p999_ms: percentile(&sorted, 99.9),
        max_ms: sorted[n - 1],
    })
}

/// Side-by-side table. The first column is the baseline for the
/// difference columns.
pub fn report(columns: &[(String, Stats)]) -> String {
    type Getter = fn(&Stats) -> f64;
    let metrics: [(&str, Getter, bool); 11] = [
        ("frames", |s| s.frames as f64, false),
        ("duration s", |s| s.seconds, false),
        ("avg FPS", |s| s.avg_fps, true),
        ("1% low FPS", |s| s.low1_fps, true),
        ("0.1% low FPS", |s| s.low01_fps, true),
        ("frame time avg ms", |s| s.avg_ms, true),
        ("frame time p50 ms", |s| s.p50_ms, true),
        ("frame time p95 ms", |s| s.p95_ms, true),
        ("frame time p99 ms", |s| s.p99_ms, true),
        ("frame time p99.9 ms", |s| s.p999_ms, true),
        ("frame time max ms", |s| s.max_ms, true),
    ];
    let Some((base_name, base)) = columns.first() else {
        return String::new();
    };

    // Build all cells first, then pad every column to its widest cell.
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut head = vec!["metric".to_string()];
    for (i, (name, _)) in columns.iter().enumerate() {
        head.push(name.clone());
        if i > 0 {
            head.push(format!("vs {base_name}"));
        }
    }
    table.push(head);
    for (label, get, compare) in metrics {
        let mut row = vec![label.to_string()];
        for (i, (_, s)) in columns.iter().enumerate() {
            let v = get(s);
            row.push(if label == "frames" { format!("{v:.0}") } else { format!("{v:.2}") });
            if i > 0 {
                let b = get(base);
                row.push(if compare && b != 0.0 {
                    format!("{:+.1}%", 100.0 * (v - b) / b)
                } else {
                    String::new()
                });
            }
        }
        table.push(row);
    }
    let widths: Vec<usize> = (0..table[0].len())
        .map(|c| table.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for (r, row) in table.iter().enumerate() {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(c, cell)| {
                if c == 0 {
                    format!("{cell:<w$}", w = widths[c])
                } else {
                    format!("{cell:>w$}", w = widths[c])
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
        if r == 0 {
            out.push_str(&"-".repeat(widths.iter().sum::<usize>() + 2 * (widths.len() - 1)));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn parses_default_presentmon_columns() {
        let csv = "Application,ProcessID,SwapChainAddress,MsBetweenPresents,MsInPresentAPI\n\
                   game.exe,100,0x1,10.5,0.2\n\
                   game.exe,100,0x1,11.5,0.2\n";
        assert_eq!(parse_csv(csv).unwrap(), [10.5, 11.5]);
    }

    #[test]
    fn parses_v2_frame_time_column_and_bom() {
        let csv =
            "\u{feff}Application,ProcessID,FrameTime\r\ngame.exe,1,16.0\r\ngame.exe,1,17.0\r\n";
        assert_eq!(parse_csv(csv).unwrap(), [16.0, 17.0]);
    }

    #[test]
    fn takes_the_process_with_the_most_frames() {
        let csv = "Application,ProcessID,MsBetweenPresents\n\
                   game.exe,7,900\n\
                   game.exe,8,10\ngame.exe,8,10\ngame.exe,8,10\n\
                   dwm.exe,9,16\ndwm.exe,9,16\n";
        assert_eq!(parse_csv(csv).unwrap(), [10.0, 10.0, 10.0]);
    }

    #[test]
    fn skips_unusable_rows() {
        let csv = "Application,ProcessID,MsBetweenPresents\n\
                   game.exe,1,NA\ngame.exe,1,0\ngame.exe,1,-3\ngame.exe,1\ngame.exe,1,12\n\n";
        assert_eq!(parse_csv(csv).unwrap(), [12.0]);
    }

    #[test]
    fn reports_what_is_wrong_with_a_capture() {
        assert!(parse_csv("").unwrap_err().contains("empty"));
        assert!(parse_csv("a,b\n1,2\n").unwrap_err().contains("MsBetweenPresents"));
        assert!(parse_csv("Application,MsBetweenPresents\n").unwrap_err().contains("no frames"));
    }

    #[test]
    fn steady_capture() {
        let s = stats(&[10.0; 1000]).unwrap();
        assert_eq!(s.frames, 1000);
        assert!(close(s.seconds, 10.0));
        assert!(close(s.avg_fps, 100.0));
        assert!(close(s.low1_fps, 100.0));
        assert!(close(s.p99_ms, 10.0));
        assert!(close(s.max_ms, 10.0));
    }

    #[test]
    fn stutter_shows_in_the_lows_not_in_the_median() {
        // 990 frames at 10 ms, 10 frames at 50 ms.
        let mut frames = vec![10.0; 990];
        frames.extend([50.0; 10]);
        let s = stats(&frames).unwrap();
        assert!(close(s.p50_ms, 10.0));
        assert!(close(s.p95_ms, 10.0));
        assert!(close(s.p999_ms, 50.0));
        assert!(close(s.max_ms, 50.0));
        assert!(close(s.low1_fps, 20.0), "slowest 1 % are the ten 50 ms frames");
        assert!(close(s.avg_fps, 1000.0 * 1000.0 / 10400.0));
        // p99 sits between the last 10 ms and the first 50 ms frame.
        assert!(s.p99_ms > 10.0 && s.p99_ms < 50.0);
    }

    #[test]
    fn percentiles_interpolate() {
        let s = stats(&[40.0, 10.0, 30.0, 20.0]).unwrap();
        assert!(close(s.p50_ms, 25.0));
        assert!(close(s.max_ms, 40.0));
        assert!(close(s.low1_fps, 25.0), "at least one frame counts as the slowest");
    }

    #[test]
    fn single_frame_and_empty() {
        assert!(stats(&[]).is_none());
        let s = stats(&[20.0]).unwrap();
        assert!(close(s.p50_ms, 20.0) && close(s.avg_fps, 50.0));
    }

    #[test]
    fn report_compares_against_the_first_column() {
        let a = stats(&[10.0; 100]).unwrap();
        let b = stats(&[20.0; 100]).unwrap();
        let text = report(&[("Cache".into(), a), ("Frequency".into(), b)]);
        let line = |label: &str| text.lines().find(|l| l.starts_with(label)).unwrap().to_string();
        assert!(text.lines().next().unwrap().contains("vs Cache"));
        assert!(line("avg FPS").contains("100.00"));
        assert!(line("avg FPS").contains("50.00"));
        assert!(line("avg FPS").contains("-50.0%"));
        assert!(line("frame time p99 ms").contains("+100.0%"));
        assert!(!line("frames").contains('%'));
    }
}

//! Frame-time captures: reading PresentMon CSV files and turning frame
//! times into the usual statistics. Pure functions, no OS access.

/// One process in a capture.
struct Group {
    application: String,
    pid: String,
    times: Vec<f64>,
}

/// Frame times per process, in order of first appearance.
///
/// The frame time column is `MsBetweenPresents` (default metrics) or
/// `FrameTime` (2.x metrics).
fn groups(text: &str) -> Result<Vec<Group>, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("the capture is empty")?;
    let cols: Vec<String> =
        header.trim_start_matches('\u{feff}').split(',').map(|c| c.trim().to_lowercase()).collect();
    let time_col = cols
        .iter()
        .position(|c| c == "frametime" || c == "msbetweenpresents")
        .ok_or("the capture has no FrameTime or MsBetweenPresents column")?;
    let pid_col = cols.iter().position(|c| c == "processid");
    let app_col = cols.iter().position(|c| c == "application" || c == "processname");

    let mut groups: Vec<Group> = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        let Some(value) = fields.get(time_col).and_then(|v| v.trim().parse::<f64>().ok()) else {
            continue;
        };
        if !value.is_finite() || value <= 0.0 {
            continue;
        }
        let field = |col: Option<usize>| col.and_then(|i| fields.get(i)).map_or("", |f| f.trim());
        let (app, pid) = (field(app_col), field(pid_col));
        match groups.iter_mut().find(|g| g.pid == pid && g.application.eq_ignore_ascii_case(app)) {
            Some(g) => g.times.push(value),
            None => groups.push(Group {
                application: app.to_string(),
                pid: pid.to_string(),
                times: vec![value],
            }),
        }
    }
    Ok(groups)
}

/// Frame times in milliseconds of `target` (an executable file name) from a
/// PresentMon CSV of the whole system.
///
/// Games often have helper processes with the same name that present
/// nothing, so of the processes called `target` the one with the most
/// frames is taken. When `target` presented no frames, the error names the
/// programs that did: that tells a game that was not in front from a
/// capture that did not work at all.
pub fn parse_csv(text: &str, target: &str) -> Result<Vec<f64>, String> {
    let groups = groups(text)?;
    if groups.is_empty() {
        return Err("the capture contains no frames from any program".to_string());
    }
    // A capture without a name column holds whatever was asked for.
    let named = groups.iter().any(|g| !g.application.is_empty());
    let best = groups
        .iter()
        .filter(|g| !named || g.application.eq_ignore_ascii_case(target))
        .max_by_key(|g| g.times.len());
    if let Some(g) = best {
        return Ok(g.times.clone());
    }
    // Total frames per program, busiest first.
    let mut seen: Vec<(String, usize)> = Vec::new();
    for g in &groups {
        match seen.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(&g.application)) {
            Some((_, n)) => *n += g.times.len(),
            None => seen.push((g.application.clone(), g.times.len())),
        }
    }
    seen.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let list: Vec<String> =
        seen.iter().take(6).map(|(name, n)| format!("{name} ({n} frames)")).collect();
    Err(format!(
        "{target} presented no frames during the capture. Programs that did: {}",
        list.join(", ")
    ))
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

/// What a benchmark says, in one or two sentences.
#[derive(Debug, PartialEq)]
pub struct Verdict {
    /// The profile that is faster beyond doubt, if there is one.
    pub winner: Option<String>,
    pub text: String,
}

/// Decide from average FPS: `overall` per profile (all rounds combined)
/// and `rounds` as (round, profile, avg FPS) per capture.
///
/// A profile wins when it is ahead of every other one in every round, and
/// its lead is larger than the spread between rounds of the same profile
/// (at least 2 %). With a single round the spread is unknown; 3 % is
/// required then.
pub fn verdict(overall: &[(String, f64)], rounds: &[(u32, String, f64)]) -> Verdict {
    let mut ranked: Vec<&(String, f64)> = overall.iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (Some((best, best_fps)), Some((second, second_fps))) = (ranked.first(), ranked.get(1))
    else {
        return Verdict { winner: None, text: "Nothing to compare.".into() };
    };
    let margin = 100.0 * (best_fps - second_fps) / second_fps;

    let mut ids: Vec<u32> = rounds.iter().map(|r| r.0).collect();
    ids.sort_unstable();
    ids.dedup();
    let fps = |round: u32, profile: &str| {
        rounds.iter().find(|r| r.0 == round && r.1 == profile).map(|r| r.2)
    };
    let won = ids
        .iter()
        .filter(|&&r| {
            let Some(b) = fps(r, best) else { return false };
            overall.iter().filter(|(p, _)| p != best).all(|(p, _)| fps(r, p).is_some_and(|o| b > o))
        })
        .count();
    let n = ids.len();

    // Largest spread between the rounds of one profile, in percent.
    let spread = (n >= 2).then(|| {
        overall
            .iter()
            .map(|(p, _)| {
                let v: Vec<f64> = rounds.iter().filter(|r| &r.1 == p).map(|r| r.2).collect();
                let max = v.iter().cloned().fold(f64::MIN, f64::max);
                let min = v.iter().cloned().fold(f64::MAX, f64::min);
                let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
                if mean > 0.0 {
                    100.0 * (max - min) / mean
                } else {
                    0.0
                }
            })
            .fold(0.0, f64::max)
    });
    let threshold = spread.map_or(3.0, |s| s.max(2.0));
    let versus =
        if overall.len() > 2 { format!("the next best, {second}") } else { second.clone() };

    if won < n {
        return Verdict {
            winner: None,
            text: format!(
                "No clear winner: {best} is {margin:.1}% ahead overall, but the rounds \
                 disagree ({best} was fastest in {won} of {n})."
            ),
        };
    }
    if margin <= threshold {
        let within = match spread {
            Some(s) => format!("within the {s:.1}% the rounds of one profile varied by"),
            None => "too little to tell from a single round".to_string(),
        };
        return Verdict {
            winner: None,
            text: format!(
                "No clear winner: {best} is only {margin:.1}% ahead of {versus}, {within}."
            ),
        };
    }
    let certainty = match spread {
        Some(s) => {
            format!("The rounds of one profile varied by up to {s:.1}%, so this is a clear result.")
        }
        None => "Only one round was run: run two or more to see how much results vary.".into(),
    };
    Verdict {
        winner: Some(best.clone()),
        text: format!(
            "{best} is {margin:.1}% faster than {versus} (average FPS) and was fastest in \
             {won} of {n} rounds. {certainty}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(round: u32, profile: &str, fps: f64) -> (u32, String, f64) {
        (round, profile.to_string(), fps)
    }
    fn o(profile: &str, fps: f64) -> (String, f64) {
        (profile.to_string(), fps)
    }

    #[test]
    fn a_consistent_lead_beyond_the_noise_wins() {
        // The second Longshot run.
        let rounds = [
            r(1, "Cache", 72.7),
            r(1, "Frequency", 67.0),
            r(2, "Frequency", 66.2),
            r(2, "Cache", 71.1),
        ];
        let v = verdict(&[o("Cache", 71.9), o("Frequency", 66.63)], &rounds);
        assert_eq!(v.winner.as_deref(), Some("Cache"));
        assert!(v.text.starts_with("Cache is 7.9% faster than Frequency"), "{}", v.text);
        assert!(v.text.contains("2 of 2 rounds") && v.text.contains("2.2%"), "{}", v.text);
    }

    #[test]
    fn rounds_that_disagree_are_inconclusive() {
        let rounds = [
            r(1, "Cache", 60.0),
            r(1, "Frequency", 50.0),
            r(2, "Frequency", 55.0),
            r(2, "Cache", 52.0),
        ];
        let v = verdict(&[o("Cache", 56.0), o("Frequency", 52.5)], &rounds);
        assert_eq!(v.winner, None);
        assert!(v.text.contains("rounds disagree") && v.text.contains("1 of 2"), "{}", v.text);
    }

    #[test]
    fn a_lead_smaller_than_the_spread_is_inconclusive() {
        let rounds = [r(1, "A", 100.0), r(1, "B", 99.0), r(2, "B", 95.0), r(2, "A", 96.0)];
        let v = verdict(&[o("A", 98.0), o("B", 97.0)], &rounds);
        assert_eq!(v.winner, None);
        assert!(v.text.contains("only 1.0% ahead of B"), "{}", v.text);
    }

    #[test]
    fn small_but_steady_leads_still_need_two_percent() {
        let rounds = [r(1, "A", 101.5), r(1, "B", 100.0), r(2, "B", 100.0), r(2, "A", 101.5)];
        assert_eq!(verdict(&[o("A", 101.5), o("B", 100.0)], &rounds).winner, None);
    }

    #[test]
    fn one_round_needs_three_percent_and_says_so() {
        let v = verdict(&[o("A", 110.0), o("B", 100.0)], &[r(1, "A", 110.0), r(1, "B", 100.0)]);
        assert_eq!(v.winner.as_deref(), Some("A"));
        assert!(v.text.contains("Only one round"), "{}", v.text);
        let v = verdict(&[o("A", 102.0), o("B", 100.0)], &[r(1, "A", 102.0), r(1, "B", 100.0)]);
        assert_eq!(v.winner, None);
    }

    #[test]
    fn three_profiles_compare_against_the_next_best() {
        let rounds = [r(1, "A", 120.0), r(1, "B", 100.0), r(1, "C", 110.0)];
        let v = verdict(&[o("A", 120.0), o("B", 100.0), o("C", 110.0)], &rounds);
        assert_eq!(v.winner.as_deref(), Some("A"));
        assert!(v.text.contains("than the next best, C"), "{}", v.text);
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn parses_default_presentmon_columns() {
        let csv = "Application,ProcessID,SwapChainAddress,MsBetweenPresents,MsInPresentAPI\n\
                   game.exe,100,0x1,10.5,0.2\n\
                   game.exe,100,0x1,11.5,0.2\n";
        assert_eq!(parse_csv(csv, "game.exe").unwrap(), [10.5, 11.5]);
    }

    #[test]
    fn parses_v2_frame_time_column_and_bom() {
        let csv =
            "\u{feff}Application,ProcessID,FrameTime\r\ngame.exe,1,16.0\r\ngame.exe,1,17.0\r\n";
        assert_eq!(parse_csv(csv, "game.exe").unwrap(), [16.0, 17.0]);
    }

    #[test]
    fn takes_the_process_with_the_most_frames() {
        let csv = "Application,ProcessID,MsBetweenPresents\n\
                   game.exe,7,900\n\
                   game.exe,8,10\ngame.exe,8,10\ngame.exe,8,10\n\
                   dwm.exe,9,16\ndwm.exe,9,16\n";
        assert_eq!(parse_csv(csv, "game.exe").unwrap(), [10.0, 10.0, 10.0]);
    }

    #[test]
    fn picks_the_target_out_of_a_capture_of_everything() {
        let csv = "Application,ProcessID,MsBetweenPresents\n\
                   dwm.exe,9,16\ndwm.exe,9,16\ndwm.exe,9,16\ndwm.exe,9,16\n\
                   Game.EXE,8,10\ngame.exe,8,11\n\
                   game.exe,7,500\n";
        // Not the busiest program overall, and not the helper process.
        assert_eq!(parse_csv(csv, "game.exe").unwrap(), [10.0, 11.0]);
    }

    #[test]
    fn names_who_presented_when_the_target_did_not() {
        let csv = "Application,ProcessID,MsBetweenPresents\n\
                   dwm.exe,9,16\ndwm.exe,9,16\ndwm.exe,9,16\n\
                   claude.exe,4,33\n";
        let err = parse_csv(csv, "game.exe").unwrap_err();
        assert!(err.contains("game.exe presented no frames"), "{err}");
        assert!(err.contains("dwm.exe (3 frames), claude.exe (1 frames)"), "{err}");
    }

    #[test]
    fn capture_without_a_name_column_is_taken_as_is() {
        let csv = "ProcessID,FrameTime\n1,16.0\n1,17.0\n";
        assert_eq!(parse_csv(csv, "game.exe").unwrap(), [16.0, 17.0]);
    }

    #[test]
    fn skips_unusable_rows() {
        let csv = "Application,ProcessID,MsBetweenPresents\n\
                   game.exe,1,NA\ngame.exe,1,0\ngame.exe,1,-3\ngame.exe,1\ngame.exe,1,12\n\n";
        assert_eq!(parse_csv(csv, "game.exe").unwrap(), [12.0]);
    }

    #[test]
    fn reports_what_is_wrong_with_a_capture() {
        assert!(parse_csv("", "game.exe").unwrap_err().contains("empty"));
        assert!(parse_csv("a,b\n1,2\n", "game.exe").unwrap_err().contains("MsBetweenPresents"));
        assert!(parse_csv("Application,MsBetweenPresents\n", "game.exe")
            .unwrap_err()
            .contains("no frames"));
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

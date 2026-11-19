//! Shared pieces of `pact run`'s and `pact fleet`'s end-of-run reports
//! (issue #463): agent prose capped to one line, code facts as diff stats.

use pact_vcs::FileStat;

/// Longest a line of agent prose may be in a report.
pub const PROSE_MAX: usize = 160;

/// `text` on one line, whitespace collapsed, cut to at most `max`
/// characters with `...` at the end when it was longer.
pub fn cap_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let kept: String = line.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

/// `cap_line` at `PROSE_MAX`, pointing at `pact inspect <id>` for the
/// full text when it had to cut.
pub fn cap_prose(text: &str, id: &str) -> String {
    let line = cap_line(text, usize::MAX);
    if line.chars().count() <= PROSE_MAX {
        return line;
    }
    let pointer = format!(" (pact inspect {id})");
    format!("{}{pointer}", cap_line(&line, PROSE_MAX.saturating_sub(pointer.chars().count())))
}

/// `N files +A -B`; binary files count as files but add no lines.
pub fn changes_summary(stats: &[FileStat]) -> String {
    let added: u64 = stats.iter().filter_map(|s| s.added).sum();
    let removed: u64 = stats.iter().filter_map(|s| s.removed).sum();
    format!("{} file{} +{added} -{removed}", stats.len(), if stats.len() == 1 { "" } else { "s" })
}

/// `changed: N files, +A -B`, then one `  path  +a -b` line per file up
/// to `shown`, then `  ... and K more`.
pub fn diff_stat_lines(stats: &[FileStat], shown: usize) -> Vec<String> {
    let added: u64 = stats.iter().filter_map(|s| s.added).sum();
    let removed: u64 = stats.iter().filter_map(|s| s.removed).sum();
    let mut lines =
        vec![format!("changed: {} file{}, +{added} -{removed}", stats.len(), if stats.len() == 1 { "" } else { "s" })];
    let width = stats.iter().take(shown).map(|s| s.path.chars().count()).max().unwrap_or(0);
    for stat in stats.iter().take(shown) {
        let counts = match (stat.added, stat.removed) {
            (Some(added), Some(removed)) => format!("+{added} -{removed}"),
            _ => "-".to_string(),
        };
        lines.push(format!("  {:width$}  {counts}", stat.path));
    }
    if stats.len() > shown {
        lines.push(format!("  ... and {} more", stats.len() - shown));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(path: &str, added: Option<u64>, removed: Option<u64>) -> FileStat {
        FileStat { path: path.to_string(), added, removed }
    }

    #[test]
    fn cap_line_collapses_whitespace_and_keeps_short_text() {
        assert_eq!(cap_line("  two\n  lines\tof  text ", 160), "two lines of text");
        assert_eq!(cap_line("exactly10!", 10), "exactly10!");
    }

    #[test]
    fn cap_line_cuts_on_a_char_boundary_with_a_marker() {
        assert_eq!(cap_line("abcdefghijkl", 10), "abcdefg...");
        let cut = cap_line(&"é".repeat(50), 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with("é..."), "{cut}");
        assert_eq!(cap_line("word word word", 8), "word...", "no space before the marker");
    }

    #[test]
    fn cap_prose_points_at_pact_inspect_only_when_it_cuts() {
        assert_eq!(cap_prose("short note", "task"), "short note");
        let capped = cap_prose(&"n".repeat(400), "my-task");
        assert!(capped.ends_with("... (pact inspect my-task)"), "{capped}");
        assert_eq!(capped.chars().count(), PROSE_MAX);
    }

    #[test]
    fn diff_stat_lines_total_the_files_and_list_at_most_shown() {
        let stats = vec![stat("a.rs", Some(3), Some(1)), stat("logo.png", None, None), stat("b.rs", Some(2), Some(0))];
        assert_eq!(
            diff_stat_lines(&stats, 2),
            vec!["changed: 3 files, +5 -1", "  a.rs      +3 -1", "  logo.png  -", "  ... and 1 more"]
        );
        assert_eq!(changes_summary(&stats[..1]), "1 file +3 -1");
        assert_eq!(diff_stat_lines(&[], 20), vec!["changed: 0 files, +0 -0"]);
    }
}

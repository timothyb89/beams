//! The workstation path picker: a dropdown under a path field listing the
//! entries of the directory typed so far, fuzzy-filtered by the segment
//! being typed (so `dl` finds `Downloads`), plus recently used destinations
//! while the field is still empty.
//!
//! The picker only computes candidates; `form.rs` owns the keys (↑/↓ to
//! highlight, Tab to complete, Enter to take a highlighted entry, Esc to
//! hide) and rendering.

use std::path::{Path, PathBuf};

use super::form::{resolve_local_path, Completion};

/// Directory entries read per listing; bigger directories are truncated.
const MAX_ENTRIES: usize = 4096;
/// Longest pattern or name the fuzzy matcher scores (anything longer only
/// gets a prefix check), keeping the per-keystroke cost bounded.
const MAX_SCORED_CHARS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Directory,
    Recent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// What the row shows: an entry name (with `/` for directories) or a
    /// whole recent path.
    pub label: String,
    /// Character indices in `label` that matched the typed text.
    pub positions: Vec<usize>,
    /// The field's full text after choosing this item.
    pub replacement: String,
    pub is_dir: bool,
    pub source: Source,
}

#[derive(Clone, Debug, Default)]
pub struct Picker {
    kind: Option<Completion>,
    recents: Vec<String>,
    /// The text `items` were computed for.
    computed_for: Option<String>,
    listing: Option<(PathBuf, Vec<(String, bool)>)>,
    pub items: Vec<Item>,
    pub highlighted: Option<usize>,
    /// Hidden with Esc until the text changes.
    pub dismissed: bool,
}

impl Picker {
    pub fn new(kind: Completion) -> Self {
        Self {
            kind: Some(kind),
            ..Self::default()
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.computed_for = None;
    }

    pub fn visible(&self) -> bool {
        !self.dismissed && !self.items.is_empty()
    }

    /// Recomputes the candidates if `text` changed since the last call;
    /// returns whether it did.
    pub fn refresh(&mut self, text: &str) -> bool {
        if self.computed_for.as_deref() == Some(text) {
            return false;
        }
        self.computed_for = Some(text.to_string());
        self.highlighted = None;
        self.dismissed = false;
        let Some(kind) = self.kind else {
            self.items.clear();
            return true;
        };
        if text.is_empty() {
            self.items = self
                .recents
                .iter()
                .map(|recent| Item {
                    label: recent.clone(),
                    positions: Vec::new(),
                    replacement: recent.clone(),
                    is_dir: true,
                    source: Source::Recent,
                })
                .collect();
            return true;
        }
        if text == "~" {
            self.items = vec![Item {
                label: "~/".to_string(),
                positions: vec![0],
                replacement: "~/".to_string(),
                is_dir: true,
                source: Source::Directory,
            }];
            return true;
        }
        let (dir_text, segment) = match text.rfind('/') {
            Some(index) => (&text[..=index], &text[index + 1..]),
            None => ("", text),
        };
        let dir = resolve_local_path(if dir_text.is_empty() { "." } else { dir_text });
        if self.listing.as_ref().map(|(path, _)| path) != Some(&dir) {
            self.listing = Some((dir.clone(), list_dir(&dir)));
        }
        let entries = &self.listing.as_ref().expect("listing just set").1;
        self.items = filter(entries, dir_text, segment, kind);
        true
    }

    pub fn highlight_next(&mut self, step: usize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() - 1;
        self.highlighted = Some(match self.highlighted {
            None => (step - 1).min(last),
            Some(index) => (index + step).min(last),
        });
    }

    /// Moves the highlight up; from the first row it goes back to the text.
    pub fn highlight_previous(&mut self, step: usize) {
        self.highlighted = match self.highlighted {
            None | Some(0) => None,
            Some(index) => Some(index.saturating_sub(step)),
        };
    }
}

fn list_dir(dir: &Path) -> Vec<(String, bool)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .take(MAX_ENTRIES)
        .flatten()
        .map(|entry| {
            // Follow symlinks so a link to a directory lists like one.
            let is_dir = std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir());
            (entry.file_name().to_string_lossy().into_owned(), is_dir)
        })
        .collect()
}

fn filter(
    entries: &[(String, bool)],
    dir_text: &str,
    segment: &str,
    kind: Completion,
) -> Vec<Item> {
    let show_hidden = segment.starts_with('.');
    let mut scored: Vec<(i32, Item)> = entries
        .iter()
        .filter(|(name, is_dir)| {
            (show_hidden || !name.starts_with('.')) && (kind == Completion::Paths || *is_dir)
        })
        .filter_map(|(name, is_dir)| {
            let (score, positions) = if segment.is_empty() {
                (0, Vec::new())
            } else {
                fuzzy_match(segment, name)?
            };
            let mut label = name.clone();
            if *is_dir {
                label.push('/');
            }
            Some((
                score,
                Item {
                    replacement: format!("{dir_text}{label}"),
                    label,
                    positions,
                    is_dir: *is_dir,
                    source: Source::Directory,
                },
            ))
        })
        .collect();
    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            // An empty segment lists directories first; ties after a real
            // match prefer the shorter name ("src" over "src-old").
            .then_with(|| {
                if segment.is_empty() {
                    b.is_dir.cmp(&a.is_dir)
                } else {
                    a.label.len().cmp(&b.label.len())
                }
            })
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.label.cmp(&b.label))
    });
    scored.into_iter().map(|(_, item)| item).collect()
}

const SCORE_MATCH: i32 = 16;
const BONUS_START: i32 = 12;
const BONUS_BOUNDARY: i32 = 10;
const BONUS_CAMEL: i32 = 8;
const BONUS_CONSECUTIVE: i32 = 6;
const PENALTY_GAP: i32 = 1;
const PENALTY_LEADING: i32 = 1;
const MAX_LEADING_PENALTY: i32 = 6;

fn is_boundary(ch: char) -> bool {
    matches!(ch, '-' | '_' | '.' | ' ' | '/')
}

/// Scores `candidate` against a fuzzy `pattern`: every pattern character
/// must appear in order. Matches at the start, after `-`/`_`/`.`/space, at
/// camelCase humps, and in consecutive runs score higher; gaps cost a
/// little. Case-insensitive unless the pattern has an uppercase letter.
/// Returns the score and the matched character indices.
pub fn fuzzy_match(pattern: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let case_sensitive = pattern.chars().any(char::is_uppercase);
    let fold = |ch: char| {
        if case_sensitive {
            ch
        } else {
            ch.to_lowercase().next().unwrap_or(ch)
        }
    };
    let pattern: Vec<char> = pattern.chars().map(fold).collect();
    let original: Vec<char> = candidate.chars().collect();
    let text: Vec<char> = original.iter().copied().map(fold).collect();
    let (m, n) = (pattern.len(), text.len());
    if m == 0 {
        return Some((0, Vec::new()));
    }
    if m > n {
        return None;
    }
    if m > MAX_SCORED_CHARS || n > MAX_SCORED_CHARS {
        let prefix = text.starts_with(&pattern);
        return prefix.then(|| (SCORE_MATCH * m as i32, (0..m).collect()));
    }
    // Cheap rejection before the quadratic pass.
    let mut rest = text.iter();
    if !pattern.iter().all(|p| rest.any(|t| t == p)) {
        return None;
    }

    let bonus: Vec<i32> = (0..n)
        .map(|j| {
            if j == 0 {
                BONUS_START
            } else if is_boundary(original[j - 1]) {
                BONUS_BOUNDARY
            } else if original[j - 1].is_lowercase() && original[j].is_uppercase() {
                BONUS_CAMEL
            } else {
                0
            }
        })
        .collect();

    // best[i][j]: the best score with pattern[..=i] matched and pattern[i]
    // at text[j]; from[i][j]: where pattern[i - 1] sat for that score.
    const NONE: i32 = i32::MIN / 2;
    let mut best = vec![vec![NONE; n]; m];
    let mut from = vec![vec![0usize; n]; m];
    for j in 0..n {
        if text[j] == pattern[0] {
            let leading = (j as i32 * PENALTY_LEADING).min(MAX_LEADING_PENALTY);
            best[0][j] = SCORE_MATCH + bonus[j] - leading;
        }
    }
    for i in 1..m {
        // The best predecessor seen so far, already charged for the gap up
        // to the current column.
        let mut gap_best = NONE;
        let mut gap_from = 0;
        for j in i..n {
            // Moving one column right lengthens any earlier gap by one.
            if j >= 2 && best[i - 1][j - 2] > NONE {
                let candidate = best[i - 1][j - 2] - PENALTY_GAP;
                if candidate > gap_best - PENALTY_GAP {
                    gap_best = candidate;
                    gap_from = j - 2;
                } else {
                    gap_best -= PENALTY_GAP;
                }
            } else if gap_best > NONE {
                gap_best -= PENALTY_GAP;
            }
            if text[j] != pattern[i] {
                continue;
            }
            let adjacent = best[i - 1][j - 1];
            let (previous, origin) = if adjacent > NONE && adjacent + BONUS_CONSECUTIVE >= gap_best
            {
                (adjacent + BONUS_CONSECUTIVE, j - 1)
            } else {
                (gap_best, gap_from)
            };
            if previous > NONE {
                best[i][j] = previous + SCORE_MATCH + bonus[j];
                from[i][j] = origin;
            }
        }
    }
    let (end, &score) = best[m - 1]
        .iter()
        .enumerate()
        .max_by_key(|(j, score)| (**score, std::cmp::Reverse(*j)))?;
    if score <= NONE {
        return None;
    }
    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = j;
        if i > 0 {
            j = from[i][j];
        }
    }
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(pattern: &str, names: &[&str]) -> Vec<String> {
        let mut scored: Vec<(i32, &str)> = names
            .iter()
            .filter_map(|name| fuzzy_match(pattern, name).map(|(score, _)| (score, *name)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
        scored
            .into_iter()
            .map(|(_, name)| name.to_string())
            .collect()
    }

    #[test]
    fn fuzzy_match_requires_every_character_in_order() {
        assert!(fuzzy_match("dl", "Downloads").is_some());
        assert!(fuzzy_match("ld", "Downloads").is_some());
        assert!(fuzzy_match("dlx", "Downloads").is_none());
        assert!(fuzzy_match("D", "downloads").is_none(), "smart case");
        assert_eq!(fuzzy_match("", "x"), Some((0, Vec::new())));
    }

    #[test]
    fn fuzzy_match_reports_the_best_positions() {
        assert_eq!(fuzzy_match("dl", "Downloads").unwrap().1, vec![0, 4]);
        // Word boundaries beat an earlier scattered match.
        assert_eq!(fuzzy_match("pb", "project-beta").unwrap().1, vec![0, 8]);
        assert_eq!(fuzzy_match("fb", "fooBar").unwrap().1, vec![0, 3]);
        assert_eq!(fuzzy_match("doc", "Documents").unwrap().1, vec![0, 1, 2]);
    }

    #[test]
    fn fuzzy_ranking_prefers_prefixes_runs_and_boundaries() {
        assert_eq!(
            rank("doc", &["Downloads", "Documents", "my-docs", "adhoc"]),
            vec!["Documents", "my-docs", "adhoc"]
        );
        assert_eq!(
            rank("src", &["src-old", "src", "resource"]),
            vec!["src", "src-old", "resource"]
        );
        assert_eq!(rank("pb", &["probe", "project-beta"])[0], "project-beta");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beams-picker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Downloads")).unwrap();
        std::fs::create_dir_all(dir.join("Documents")).unwrap();
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        std::fs::write(dir.join("download.log"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        dir
    }

    fn labels(picker: &Picker) -> Vec<&str> {
        picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect()
    }

    #[test]
    fn picker_lists_and_filters_the_typed_directory() {
        let dir = scratch("list");
        let base = format!("{}/", dir.display());
        let mut picker = Picker::new(Completion::Paths);

        picker.refresh(&base);
        assert_eq!(
            labels(&picker),
            vec!["Documents/", "Downloads/", "download.log", "notes.txt"],
            "directories first, hidden entries left out"
        );

        picker.refresh(&format!("{base}dl"));
        assert_eq!(labels(&picker), vec!["download.log", "Downloads/"]);
        assert_eq!(picker.items[1].replacement, format!("{base}Downloads/"));
        assert_eq!(picker.items[1].positions, vec![0, 4]);

        picker.refresh(&format!("{base}.c"));
        assert_eq!(labels(&picker), vec![".config/"]);

        let mut dirs = Picker::new(Completion::Directories);
        dirs.refresh(&format!("{base}do"));
        assert_eq!(labels(&dirs), vec!["Documents/", "Downloads/"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn picker_offers_recents_while_empty_and_resets_on_edit() {
        let mut picker = Picker::new(Completion::Directories);
        picker.set_recents(vec!["~/Downloads/".to_string()]);
        picker.refresh("");
        assert_eq!(labels(&picker), vec!["~/Downloads/"]);
        assert_eq!(picker.items[0].source, Source::Recent);

        picker.highlight_next(1);
        assert_eq!(picker.highlighted, Some(0));
        picker.highlight_next(1);
        assert_eq!(picker.highlighted, Some(0), "stops at the last row");
        picker.highlight_previous(1);
        assert_eq!(picker.highlighted, None, "back to the text");

        picker.highlight_next(1);
        picker.dismissed = true;
        picker.refresh("");
        assert!(picker.dismissed, "same text keeps state");
        picker.refresh("/nonexistent-beams-dir/");
        assert!(!picker.dismissed);
        assert_eq!(picker.highlighted, None);
        assert!(!picker.visible());
    }
}

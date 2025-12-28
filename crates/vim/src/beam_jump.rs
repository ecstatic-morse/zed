use std::{ops::Range, sync::Arc};

use collections::{HashMap, HashSet};
use editor::{MultiBufferOffset, MultiBufferSnapshot};
use itertools::{Either, Itertools};
use ui::SharedString;
use workspace::searchable::Direction;

use crate::motion::{Motion, is_character_match};

type BeamJumpMatch = Range<MultiBufferOffset>;

#[derive(Debug, Clone, Copy)]
enum LabelLen {
    One = 1,
    Two = 2,
}

#[derive(Debug, Clone)]
struct BeamJumpLabelMaps {
    label_by_start: HashMap<MultiBufferOffset, SharedString>,
    start_by_label: HashMap<SharedString, MultiBufferOffset>,
    num_chars: LabelLen,
}

#[derive(Clone, Debug)]
pub(crate) struct BeamJumpState {
    pub(crate) direction: Option<Direction>,
    pub(crate) smartcase: bool,
    pub(crate) jump_origin: MultiBufferOffset,
    pub(crate) visible_range: Range<MultiBufferOffset>,
    pub(crate) previous_last_find: Option<Motion>,
    pub(crate) base_label_chars: Arc<[char]>,
    pattern: String,
    num_pattern_chars: usize,
    label_prefix: String,
    labels: Option<BeamJumpLabelMaps>,
    pub(crate) candidates: Vec<BeamJumpMatch>,
}

#[derive(Clone, Debug)]
pub(crate) struct BeamJumpJump {
    pub(crate) direction: Direction,
    pub(crate) pattern: String,
    pub(crate) smartcase: bool,
    pub(crate) count: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum BeamJumpAction {
    Continue,
    Cancel,
    PassThrough,
    Jump(BeamJumpJump),
}

impl BeamJumpLabelMaps {
    fn init(
        all_chars: &[char],
        candidates: &[BeamJumpMatch],
        valid_first_chars: &HashSet<char>,
        origin: MultiBufferOffset,
    ) -> Self {
        let mut label_by_start = HashMap::default();
        let mut start_by_label = HashMap::default();
        let candidates_by_distance: Vec<_> = candidates
            .iter()
            .sorted_unstable_by_key(|m| m.start.0.abs_diff(origin.0))
            .collect();

        let num_chars = if candidates.len() <= valid_first_chars.len() {
            LabelLen::One
        } else {
            LabelLen::Two
        };

        let labels = n_char_labels(num_chars, all_chars, |c| valid_first_chars.contains(&c));
        for (m, s) in candidates_by_distance.iter().zip(labels) {
            let l = SharedString::from(s);
            label_by_start.insert(m.start, l.clone());
            start_by_label.insert(l, m.start);
        }

        return Self {
            label_by_start,
            start_by_label,
            num_chars,
        };
    }

    // True if this is the start of one of the active labels.
    fn is_first_char_of_label(&self, ch: char) -> bool {
        self.start_by_label.keys().any(|l| l.starts_with(ch))
    }
}

impl BeamJumpState {
    pub(crate) fn new(
        direction: Option<Direction>,
        smartcase: bool,
        base_label_chars: Arc<[char]>,
        jump_origin: MultiBufferOffset,
        visible_range: Range<MultiBufferOffset>,
        previous_last_find: Option<Motion>,
    ) -> Self {
        Self {
            direction,
            smartcase,
            jump_origin,
            visible_range,
            previous_last_find,
            base_label_chars,
            pattern: Default::default(),
            num_pattern_chars: 0,
            label_prefix: Default::default(),
            candidates: Vec::new(),
            labels: None,
        }
    }

    pub(crate) fn pattern(&self) -> &str {
        self.pattern.as_str()
    }

    pub(crate) fn num_pattern_chars(&self) -> usize {
        self.num_pattern_chars
    }

    pub(crate) fn candidates_and_labels(
        &self,
    ) -> impl Iterator<Item = (BeamJumpMatch, Option<&SharedString>)> {
        self.candidates.iter().map(|m| {
            let l = self
                .labels
                .as_ref()
                .and_then(|labels| labels.label_by_start.get(&m.start));
            (m.clone(), l)
        })
    }

    pub(crate) fn search_range(&self) -> Range<MultiBufferOffset> {
        match self.direction {
            None => self.visible_range.clone(),
            Some(Direction::Next) => self.jump_origin..self.visible_range.end,
            Some(Direction::Prev) => self.visible_range.start..self.jump_origin,
        }
    }

    pub(crate) fn on_typed_char(
        &mut self,
        ch: char,
        buffer: &MultiBufferSnapshot,
    ) -> BeamJumpAction {
        let is_label_char = !self.label_prefix.is_empty()
            || self.num_pattern_chars >= 2
                && self
                    .labels
                    .as_ref()
                    .is_some_and(|l| l.is_first_char_of_label(ch));

        if is_label_char {
            return self.push_label_char(ch);
        }

        self.push_pattern_char(ch, buffer);

        if self.candidates.len() > 1 || self.num_pattern_chars() <= 1 {
            return BeamJumpAction::Continue;
        }

        let Some(m) = self.candidates.first() else {
            return BeamJumpAction::Cancel;
        };

        // If we have exactly one candidate (and have typed at least two characters), jump to it
        BeamJumpAction::Jump(BeamJumpJump {
            direction: Direction::next_if(m.start >= self.jump_origin),
            pattern: self.pattern().into(),
            smartcase: self.smartcase,
            count: 1,
        })
    }

    fn push_label_char(&mut self, ch: char) -> BeamJumpAction {
        self.label_prefix.push(ch);
        let curr_label = &self.label_prefix;

        let Some(labels) = self.labels.as_mut() else {
            return BeamJumpAction::Cancel;
        };

        if curr_label.chars().count() < labels.num_chars as usize {
            return BeamJumpAction::Continue;
        }

        let Some(pos) = labels
            .start_by_label
            .get(curr_label.as_str())
            .and_then(|&start| self.candidates.iter().position(|x| x.start == start))
        else {
            return BeamJumpAction::Cancel;
        };

        let origin = self.jump_origin.saturating_sub_usize(1);
        let m = &self.candidates[pos];
        let direction = Direction::next_if(m.start >= origin);

        // N.B. if the cursor is *within* a candidate (e.g. "a|b"), we cannot
        // jump back to the start without starting the search from a different
        // position. We don't handle this yet, so we have to ensure `count = 0`
        // for this case
        let opos = if direction == Direction::Next {
            self.candidates.binary_search_by_key(&origin, |x| x.start)
        } else {
            self.candidates.binary_search_by_key(&origin, |x| x.end)
        };

        let count = match (opos, direction) {
            _ if m.start == origin => 0,
            (Ok(o), Direction::Prev) | (Err(o), Direction::Next) => o.abs_diff(pos) + 1,
            (Ok(o), Direction::Next) | (Err(o), Direction::Prev) => o.abs_diff(pos),
        };
        return BeamJumpAction::Jump(BeamJumpJump {
            direction,
            pattern: self.pattern().into(),
            smartcase: self.smartcase,
            count: count,
        });
    }

    fn push_pattern_char(&mut self, ch: char, buffer: &MultiBufferSnapshot) {
        self.num_pattern_chars += 1;
        self.pattern.push(ch);

        if self.num_pattern_chars == 1 {
            let range = self.search_range();
            let mut start = range.start;

            self.candidates = buffer
                .text_for_range(range)
                .flat_map(|s| s.chars())
                .filter_map(|x| {
                    let end = start + x.len_utf8();
                    let ret = is_character_match(ch, x, self.smartcase).then(|| start..end);
                    start = end;
                    ret
                })
                .collect();
            return;
        }

        let mut valid_label_start_chars = self.base_label_chars_set();
        let to_remove: Vec<_> = self
            .candidates
            .extract_if(.., |m| {
                let mut chars = buffer.chars_at(m.end);
                let Some(c) = chars.next() else {
                    return true;
                };

                let new_end = m.end + c.len_utf8();
                // visible_range seems to actually be an inclusive range
                if new_end > self.visible_range.end || !is_character_match(ch, c, self.smartcase) {
                    return true;
                }

                if let Some(c) = chars.next() {
                    valid_label_start_chars.remove(&c);
                    if self.smartcase {
                        valid_label_start_chars.remove(&c.to_ascii_lowercase());
                    }
                }
                m.end = new_end;
                false
            })
            .map(|x| x.start)
            .collect();

        // The pattern is no longer ambiguous, so no need to update labels.
        // We will jump directly to the remaining candidate (if one exists) upon return.
        if self.candidates.len() <= 1 {
            self.labels = None;
            return;
        }

        // If we can reduce the label length by relabeling, do so.
        if let Some(LabelLen::Two) = self.labels.as_ref().map(|x| x.num_chars)
            && self.candidates.len() <= valid_label_start_chars.len()
        {
            self.labels = None;
        }

        let Some(ref mut labels) = self.labels else {
            self.labels = Some(BeamJumpLabelMaps::init(
                &self.base_label_chars,
                &self.candidates,
                &valid_label_start_chars,
                self.jump_origin,
            ));
            return;
        };

        // Update label maps

        for start in to_remove {
            if let Some(l) = labels.label_by_start.remove(&start) {
                labels.start_by_label.remove(&l);
            };
        }

        let to_relabel: Vec<_> = labels
            .start_by_label
            .extract_if(|l, _| {
                l.chars()
                    .next()
                    .is_some_and(|c| !valid_label_start_chars.contains(&c))
            })
            .collect();

        let mut new_labels = n_char_labels(labels.num_chars, &self.base_label_chars, |c| {
            valid_label_start_chars.contains(&c)
        });
        for (_, start) in to_relabel {
            let Some(new) = new_labels.find(|l| !labels.start_by_label.contains_key(&**l)) else {
                continue;
            };
            let new = SharedString::from(new);
            labels.label_by_start.insert(start, new.clone());
            labels.start_by_label.insert(new, start);
        }
    }

    fn base_label_chars_set(&mut self) -> HashSet<char> {
        self.base_label_chars.iter().copied().collect()
    }
}

const DEFAULT_DUPES_PENALTY: usize = 11;

fn n_char_labels<'a>(
    n: LabelLen,
    all_chars: &'a [char],
    is_valid_first_char: impl Fn(char) -> bool + 'a,
) -> impl Iterator<Item = String> + 'a {
    match n {
        LabelLen::One => Either::Left(
            all_chars
                .iter()
                .filter(move |&&c| is_valid_first_char(c))
                .map(|&c| String::from(c)),
        ),
        LabelLen::Two => Either::Right(
            two_char_labels(&all_chars, DEFAULT_DUPES_PENALTY)
                .filter(move |&(c, _)| is_valid_first_char(c))
                .map(|(c1, c2)| format!("{}{}", c1, c2)),
        ),
    }
}

// Order characters by the minimum sum of the indices of those characters in the input slice.
// e.g. aa ab ba ac bb ca ad bc ... zx yz zy zz
fn two_char_labels(all_chars: &[char], dupes_penalty: usize) -> impl Iterator<Item = (char, char)> {
    let last_idx = all_chars.len() - 1;
    let no_dupes = (0..=last_idx * 2)
        .flat_map(move |sum| {
            let start = sum.saturating_sub(last_idx);
            let end = std::cmp::min(sum, last_idx);
            (start..=end).map(move |i| (i, sum - i))
        })
        .filter(move |&(i, j)| i != j);

    let dupes = (0..all_chars.len()).map(|i| (i, i));

    no_dupes
        .merge_by(dupes, move |(a, b), (c, _)| {
            (a + b)
                .cmp(&(c + c + dupes_penalty))
                .then_with(|| a.cmp(c))
                .is_le()
        })
        .map(move |(i, j)| (all_chars[i], all_chars[j]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_char_labels_ordering() {
        let got: Vec<_> = two_char_labels(&['a', 'b', 'c', 'd', 'e'], 0)
            .map(|(a, b)| format!("{a}{b}"))
            .collect();
        assert_eq!(&got[..5], &["aa", "ab", "ba", "ac", "bb"]);
        assert_eq!(&got[20..], &["dd", "ec", "de", "ed", "ee"]);
    }

    #[test]
    fn two_char_labels_ordering_with_doubles_last() {
        let got: Vec<_> = two_char_labels(&['a', 'b', 'c', 'd', 'e'], 1000)
            .map(|(a, b)| format!("{a}{b}"))
            .collect();

        let expected: Vec<_> = two_char_labels(&['a', 'b', 'c', 'd', 'e'], 0)
            .sorted_by_key(|(a, b)| a == b)
            .map(|(a, b)| format!("{a}{b}"))
            .collect();

        assert_eq!(got, expected);
    }
}

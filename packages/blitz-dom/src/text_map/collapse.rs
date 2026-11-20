//! White space collapsing for an inline formatting context.
//!
//! White space is collapsed here (CSS Text §4.1.1) while recording a [`TextOffsetMap`], and the
//! final collapsed text and style runs are fed directly to Parley's [`StyleRunBuilder`] without
//! Parley rescanning the text.

use icu_properties::{
    CodePointMapData,
    props::{GeneralCategory, GeneralCategoryGroup},
};
use parley::{Brush, InlineBox, StyleProperty, StyleRunBuilder, TextWrapMode, WhiteSpaceCollapse};

use super::INLINE_BOX_CHAR;
use super::offset_map::{TextOffsetMap, TextOffsetMapBuilder, UnitKind};

/// Whether `c` is a segment break (CSS Text §4.1.1).
pub fn is_segment_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_collapsible(mode: WhiteSpaceCollapse, c: char) -> bool {
    match mode {
        WhiteSpaceCollapse::Collapse => c.is_ascii_whitespace(),
        WhiteSpaceCollapse::PreserveBreaks => matches!(c, ' ' | '\t'),
        WhiteSpaceCollapse::Preserve | WhiteSpaceCollapse::BreakSpaces => false,
    }
}

/// The white space handling of the span a text node is in.
#[derive(Debug, Clone, Copy)]
pub struct WhiteSpace {
    pub collapse: WhiteSpaceCollapse,
    /// The space a collapsible run collapses to: U+3000 under `text-transform: full-width`.
    pub collapsed_space: &'static str,
    /// Whether `text-wrap-mode` is `wrap`.
    pub wrap: bool,
}

/// A collapsible run that will collapse to one space if content follows it.
struct Pending {
    /// The unit of the run's first character, which becomes the kept space.
    kept_unit: usize,
    /// The style of the span where the collapsible run started.
    style_index: u16,
    space: &'static str,
    /// Whether the span where the run started allows wrapping.
    start_wrap: bool,
    /// Whether any span contributing whitespace to this run allows wrapping.
    wrap: bool,
}

/// The start of a range of a text node's transformed text, and where it came from.
#[derive(Clone, Copy)]
struct Segment {
    text: usize,
    source: usize,
    /// Whether the range maps byte for byte to the source.
    exact: bool,
}

/// Collapses the white space of an inline formatting context's text and records where each
/// part of each source node (identified by `Id`) ended up in the layout text, pushing style
/// runs and inline boxes to a Parley [`StyleRunBuilder`].
pub struct WhiteSpaceCollapser<Id> {
    map: TextOffsetMapBuilder<Id>,
    /// The collapsed text committed so far (pending white space isn't).
    text: String,
    ends_with_break: bool,
    pending: Option<Pending>,
    /// Whether the current text node's transformed text is its source, so `segments` is unused.
    identity: bool,
    /// The current text node's transformed text, aligned to its source.
    segments: Vec<Segment>,
}

impl<Id> Default for WhiteSpaceCollapser<Id> {
    fn default() -> Self {
        Self {
            map: TextOffsetMapBuilder::default(),
            text: String::new(),
            ends_with_break: false,
            pending: None,
            identity: false,
            segments: Vec::new(),
        }
    }
}

impl<Id: Copy + Ord> WhiteSpaceCollapser<Id> {
    /// Creates a collapser that clears and reuses `text`'s allocation for the collapsed output.
    pub fn with_text(mut text: String) -> Self {
        text.clear();
        Self {
            text,
            ..Self::default()
        }
    }

    /// The collapsed text committed so far (not including pending collapsible white space).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the text so far is followed by pending collapsible white space.
    pub fn has_pending_whitespace(&self) -> bool {
        self.pending.is_some()
    }

    /// Collapses and pushes the content of the text node `node` under `style_index`. `text` is
    /// `source` after `text-transform`: case mapping, then `map_char` on each character
    /// (`full-width` and `full-size-kana`).
    #[allow(clippy::too_many_arguments)]
    pub fn push_text<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        node: Id,
        source: &str,
        text: &str,
        white_space: WhiteSpace,
        map_char: impl Fn(char) -> char,
    ) {
        self.map.begin_node(node);
        if !text.is_empty() {
            self.identity =
                (source.as_ptr() == text.as_ptr() && source.len() == text.len()) || source == text;
            if !self.identity {
                align(source, text, map_char, &mut self.segments);
            }
            self.collapse(builder, style_index, text, white_space);
        }
        self.map.end_node(source.len(), self.text.len());
    }

    /// Pushes text generated for `node` (e.g. a list marker), which has no source offsets.
    pub fn push_generated<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        node: Id,
        text: &str,
        white_space: WhiteSpace,
    ) {
        self.push_text(builder, style_index, node, "", text, white_space, |c| c);
    }

    /// Pushes a forced line break element (`<br>`) under `style_index`.
    pub fn push_line_break<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        node: Id,
    ) {
        self.map.begin_node(node);
        self.pending = None;
        let start = self.text.len();
        self.map.push(UnitKind::Identity, 0, start);
        self.text.push('\n');
        builder.push_style_run(style_index, '\n'.len_utf8());
        self.ends_with_break = true;
        self.map.end_node(1, self.text.len());
    }

    /// Pushes an in-flow inline box under `style_index`.
    ///
    /// Flushes any pending collapsible white space, appends U+FFFC to the text, and pushes the
    /// box to `builder` with its `index` set to that character's byte offset.
    pub fn push_inline_box<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        node: Id,
        inline_box: InlineBox,
    ) {
        self.map.begin_node(node);
        self.flush(builder);
        let index = self.text.len();
        self.map.push(UnitKind::Variable, 0, index);
        self.text.push(INLINE_BOX_CHAR);
        builder.push_style_run(style_index, INLINE_BOX_CHAR.len_utf8());
        builder.push_inline_box(InlineBox {
            index,
            ..inline_box
        });
        self.ends_with_break = false;
        self.map.end_node(1, self.text.len());
    }

    /// Records the position of an out-of-flow box (e.g. floated or absolutely positioned), which
    /// takes up no character in the text and does not end a collapsible run.
    ///
    /// An out-of-flow box is recorded after any collapsible space before it, in DOM order,
    /// though Parley anchors it before that space (see [`TextOffsetMap::layout_range`]).
    ///
    /// Returns its byte offset in the text committed so far.
    pub fn push_out_of_flow_box(&mut self, node: Id) -> usize {
        self.map.begin_node(node);
        let index = self.text.len();
        self.map.push(UnitKind::Variable, 0, index);
        self.map.end_node(1, index);
        index
    }

    /// Finishes the formatting context, dropping any white space at its end, and returns the
    /// collapsed layout text and its offset map.
    pub fn finish(self) -> (String, TextOffsetMap<Id>) {
        (self.text, self.map.finish())
    }

    fn collapse<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        text: &str,
        white_space: WhiteSpace,
    ) {
        let mode = white_space.collapse;
        if !is_collapsible(mode, ' ') && !is_collapsible(mode, '\n') {
            // Preserved white space: only a preserved segment break drops a pending run.
            if text.starts_with(is_segment_break) {
                self.pending = None;
            }
            self.flush(builder);
            self.commit(
                builder,
                style_index,
                text,
                0..text.len(),
                UnitKind::Identity,
            );
            return;
        }

        let mut pos = 0;
        while let Some(c) = text[pos..].chars().next() {
            let rest = &text[pos..];
            let run_len = rest
                .find(|c| !is_collapsible(mode, c))
                .unwrap_or(rest.len());
            if run_len > 0 {
                self.collapse_run(pos, c.len_utf8(), run_len, style_index, white_space);
                pos += run_len;
            } else if is_segment_break(c) {
                self.pending = None;
                self.commit(
                    builder,
                    style_index,
                    text,
                    pos..pos + c.len_utf8(),
                    UnitKind::Identity,
                );
                pos += c.len_utf8();
            } else {
                let mut len = content_len(rest, mode);
                // A lone space between content in this node is kept as is, so commit it with
                // the content around it rather than as a run of its own.
                if white_space.collapsed_space == " " {
                    while rest.as_bytes().get(len) == Some(&b' ') {
                        match content_len(&rest[len + 1..], mode) {
                            0 => break,
                            after => len += 1 + after,
                        }
                    }
                }
                self.flush(builder);
                self.commit(
                    builder,
                    style_index,
                    text,
                    pos..pos + len,
                    UnitKind::CollapsibleText,
                );
                pos += len;
            }
        }
    }

    /// Records a collapsible run at `pos` whose first character is `first_len` bytes long.
    fn collapse_run(
        &mut self,
        pos: usize,
        first_len: usize,
        run_len: usize,
        style_index: u16,
        white_space: WhiteSpace,
    ) {
        let source = self.source_at(pos);
        if let Some(pending) = &mut self.pending {
            pending.wrap |= white_space.wrap;
            self.map.push(UnitKind::Variable, source, self.text.len());
            return;
        }
        let kept_unit = self.map.push(UnitKind::KeptSpace, source, self.text.len());
        self.pending = Some(Pending {
            kept_unit,
            style_index,
            space: white_space.collapsed_space,
            start_wrap: white_space.wrap,
            wrap: white_space.wrap,
        });
        if first_len < run_len {
            let source = self.source_at(pos + first_len);
            self.map.push(UnitKind::Variable, source, self.text.len());
        }
    }

    /// Resolves a pending run: it collapses to one space unless it is at the start of the
    /// formatting context or follows a preserved segment break, in which case it is removed.
    fn flush<B: Brush>(&mut self, builder: &mut StyleRunBuilder<'_, B>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if self.text.is_empty() || self.ends_with_break {
            return;
        }
        let style_index = if pending.wrap && !pending.start_wrap {
            builder.push_modified_style(
                pending.style_index,
                [StyleProperty::TextWrapMode(TextWrapMode::Wrap)],
            )
        } else {
            pending.style_index
        };
        self.text.push_str(pending.space);
        builder.push_style_run(style_index, pending.space.len());
        self.map.shift(pending.kept_unit + 1, pending.space.len());
        self.ends_with_break = false;
    }

    /// Commits `text[range]` unchanged, mapping the parts that match the source as `exact`.
    fn commit<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        text: &str,
        range: std::ops::Range<usize>,
        exact: UnitKind,
    ) {
        let layout_start = self.text.len();
        if self.identity {
            self.map.push(exact, range.start, layout_start);
            self.push_out(builder, style_index, &text[range]);
            return;
        }
        let first = self.segments.partition_point(|seg| seg.text <= range.start) - 1;
        let kind = |seg: &Segment| {
            if seg.exact { exact } else { UnitKind::Variable }
        };
        let source = self.source_at(range.start);
        self.map
            .push(kind(&self.segments[first]), source, layout_start);
        for seg in self.segments[first + 1..]
            .iter()
            .take_while(|seg| seg.text < range.end)
        {
            self.map
                .push(kind(seg), seg.source, layout_start + seg.text - range.start);
        }
        self.push_out(builder, style_index, &text[range]);
    }

    fn push_out<B: Brush>(
        &mut self,
        builder: &mut StyleRunBuilder<'_, B>,
        style_index: u16,
        text: &str,
    ) {
        self.text.push_str(text);
        builder.push_style_run(style_index, text.len());
        self.ends_with_break = text.ends_with(is_segment_break);
    }

    /// The source offset of the transformed text offset `pos`.
    fn source_at(&self, pos: usize) -> usize {
        if self.identity {
            return pos;
        }
        let index = self.segments.partition_point(|seg| seg.text <= pos) - 1;
        let seg = self.segments[index];
        if seg.exact {
            seg.source + (pos - seg.text)
        } else if pos == seg.text {
            seg.source
        } else {
            self.segments[index + 1].source
        }
    }
}

/// The length of the content (neither collapsible white space nor a segment break) at the
/// start of `text`.
fn content_len(text: &str, mode: WhiteSpaceCollapse) -> usize {
    text.find(|c| is_collapsible(mode, c) || is_segment_break(c))
        .unwrap_or(text.len())
}

/// Aligns `text` (`source` after case mapping and then `map_char`) to `source`, character by
/// character: case mappings that expand a character (`ß` → `SS`, or `Ss` when capitalized) and
/// combining marks removed by tailored casing are matched, and anything that can't be matched
/// maps as one range.
fn align(source: &str, text: &str, map_char: impl Fn(char) -> char, segments: &mut Vec<Segment>) {
    segments.clear();
    let mut push = |text: usize, source: usize, exact: bool| {
        if let Some(last) = segments.last()
            && last.exact
            && exact
            && text - last.text == source - last.source
        {
            return;
        }
        segments.push(Segment {
            text,
            source,
            exact,
        });
    };

    let (mut s, mut t) = (0, 0);
    // ASCII case mapping never changes lengths.
    if source.len() == text.len() && source.is_ascii() && text.is_ascii() {
        push(0, 0, true);
        s = source.len();
        t = text.len();
    } else {
        let general_category = CodePointMapData::<GeneralCategory>::new();
        while let (Some(c), Some(d)) = (source[s..].chars().next(), text[t..].chars().next()) {
            let rest = &text[t..];
            let expansion = matching_prefix(rest, c.to_uppercase().map(&map_char))
                .or_else(|| matching_prefix(rest, c.to_lowercase().map(&map_char)))
                .filter(|&len| len > d.len_utf8());
            if let Some(len) = expansion {
                push(t, s, false);
                t += len;
            } else if c != d && GeneralCategoryGroup::Mark.contains(general_category.get(c)) {
                push(t, s, false);
            } else {
                push(t, s, c.len_utf8() == d.len_utf8());
                t += d.len_utf8();
            }
            s += c.len_utf8();
        }
    }
    if s < source.len() || t < text.len() {
        push(t, s, false);
    }
    push(text.len(), source.len(), true);
    // The end segment is needed even when it merged into an exact one before it.
    if segments.last().is_none_or(|last| last.text != text.len()) {
        segments.push(Segment {
            text: text.len(),
            source: source.len(),
            exact: true,
        });
    }
}

/// The length of the prefix of `text` that is `mapped`, ignoring case (so that titlecasing
/// matches an uppercase expansion).
fn matching_prefix(text: &str, mapped: impl Iterator<Item = char>) -> Option<usize> {
    let mut text = text.chars();
    let mut len = 0;
    for c in mapped {
        let d = text.next()?;
        if d != c && !d.to_lowercase().eq(c.to_lowercase()) {
            return None;
        }
        len += d.len_utf8();
    }
    Some(len)
}

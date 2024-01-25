use crate::config::Config;
use crate::utils::{BoolExt, SliceExt, StrExt};
use crate::{html::*, map};
use anyhow::{bail, Context, Result};
use bumpalo::collections::Vec;
use bumpalo::Bump;
use codespan_reporting::diagnostic::{Diagnostic, Label};
use codespan_reporting::files::SimpleFile;
use codespan_reporting::term;
use codespan_reporting::term::termcolor::WriteColor;
use proc_macro2::LineColumn;
use std::mem::replace;
use std::vec::Vec as StdVec;
use syn::punctuated::Punctuated;
use syn::{spanned::Spanned, visit::Visit, Macro};
use syn::{Attribute, Item, Stmt};

fn is_skipped(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path()
            .segments
            .iter()
            .map(|x| &x.ident)
            .eq(["rustfmt", "skip"])
    })
}

fn print_break(out: &mut String, indent: usize) {
    out.reserve(indent + 1);
    out.push('\n');
    for _ in 0..indent {
        out.push(' ');
    }
}

#[derive(Debug, Clone, Copy)]
enum Comment<'src> {
    /// the initial `//` and the newline are not included
    Line(&'src str),
    /// the `/*` and `*/` are not included
    Multi(&'src str),
}

struct CommentParser<'src>(&'src str);

impl<'src> Iterator for CommentParser<'src> {
    type Item = Comment<'src>;

    fn next(&mut self) -> Option<Self::Item> {
        /// the `usize`s are offsets into `src`
        #[derive(Debug, Clone, Copy)]
        enum ParserState {
            None,
            Start(usize),
            Line(usize),
            Multi(usize),
            MultiEnd(usize),
        }

        let Self(src) = self;
        let mut state = ParserState::None;
        for (i, c) in src.char_indices() {
            match c {
                '/' => match state {
                    ParserState::None => state = ParserState::Start(i),
                    ParserState::Start(_) => state = ParserState::Line(i + 1),
                    ParserState::MultiEnd(start) => unsafe {
                        // Safety: `src[i]` is guaranteed to be '/'
                        let (extracted, rest) = src.split_at_unchecked(i + 1);
                        *src = rest;
                        return Some(Comment::Multi(extracted.get_unchecked(start..)));
                    },
                    _ => (),
                },

                '*' => match state {
                    ParserState::Start(i) => state = ParserState::Multi(i),
                    ParserState::Multi(i) => state = ParserState::MultiEnd(i),
                    _ => (),
                },

                '\n' => match state {
                    ParserState::Start(_) => state = ParserState::None,
                    ParserState::Line(start) => unsafe {
                        let (extracted, rest) = src.split_at_unchecked(i);
                        *src = rest;
                        let res = Some(Comment::Line(extracted.get_unchecked(start..)));
                        return res;
                    },
                    ParserState::MultiEnd(i) => state = ParserState::Multi(i),
                    _ => (),
                },

                _ => match state {
                    ParserState::Start(_) => state = ParserState::None,
                    ParserState::MultiEnd(i) => state = ParserState::Multi(i),
                    _ => (),
                },
            }
        }

        *src = "";
        None
    }
}

#[derive(Clone, Copy)]
pub struct Location {
    pub start: LineColumn,
    pub end: LineColumn,
}

/// Represents an object that has an associated location in the source
pub trait Located {
    fn start(&self) -> LineColumn;
    fn end(&self) -> LineColumn;
    fn loc(&self) -> Location {
        Location {
            start: self.start(),
            end: self.end(),
        }
    }
}

impl<T: Spanned> Located for T {
    fn start(&self) -> LineColumn {
        self.span().start()
    }
    fn end(&self) -> LineColumn {
        self.span().end()
    }
    fn loc(&self) -> Location {
        let span = self.span();
        Location {
            start: span.start(),
            end: span.end(),
        }
    }
}

impl Located for Location {
    fn start(&self) -> LineColumn {
        self.start
    }
    fn end(&self) -> LineColumn {
        self.end
    }
    fn loc(&self) -> Location {
        *self
    }
}

pub trait Format<'src> {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result<()>;
}

/// Stores the config and allocated memory to reuse it between reformatting
pub struct Formatter {
    config: Config,
    /// buffer for tokens stored in `FmtBlock`s
    tokens_buf: Bump,
    /// maps line number to byte offset in `input`
    offsets: StdVec<usize>,
    /// the formatted code
    output: String,
}

/// Represents text that's not yet written: text, space, or a group of those
enum FmtToken<'fmt, 'src> {
    Text(&'src str),
    /// needs special handling of the newline
    LineComment(&'src str),
    Sep,
    Block(FmtBlock<'fmt, 'src>),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Spacing {
    pub before: bool,
    pub between: bool,
    pub after: bool,
}

impl Spacing {
    const fn around(&self) -> bool {
        self.before && self.after
    }
}

/// Chains are sequences of formatting blocks that are all broken if one of them is broken; in a
/// sequence of formatting blocks, a chain will have the following shape:
/// `[..., Off, On, On, ..., On, End, Off, ...]`
/// where the words are the names of the variants of [`ChainingRule`].
/// A chain starts from a [`ChainingRule::On`] variant but ends with a [`ChainingRule::End`]; this
/// is done to make the chains declare their end on their own without having to add an addition
/// variant [`FmtToken`], and to also avoid the possibility of 2 unrelated chains getting
/// misinterpreted as 1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChainingRule {
    /// no chaining
    Off,
    /// ends a chain
    End,
    /// inside a chain
    On,
}

pub struct FmtBlock<'fmt, 'src> {
    tokens: Vec<'fmt, FmtToken<'fmt, 'src>>,
    width: usize,
    chaining_rule: ChainingRule,
    /// if `None`, the block is broken, since spacing only matters when the block isn't broken
    spacing: Option<Spacing>,
    /// offset into the source, useful for correct printing of comments
    cur_offset: usize,
}

impl<'fmt, 'src> FmtBlock<'fmt, 'src> {
    fn new(
        alloc: &'fmt Bump,
        spacing: Option<Spacing>,
        chaining: ChainingRule,
        start_offset: usize,
    ) -> Self {
        Self {
            tokens: Vec::new_in(alloc),
            width: spacing.map_or(false, |s| s.before || s.after) as usize,
            spacing,
            cur_offset: start_offset,
            chaining_rule: chaining,
        }
    }

    fn add_raw_text(&mut self, text: &'src str) {
        self.tokens.push(FmtToken::Text(text));
        self.width += text.len();
    }

    fn add_raw_space(&mut self) {
        self.tokens.push(FmtToken::Text(" "));
        self.width += 1;
    }

    fn add_line_comment(&mut self, comment: &'src str) {
        self.tokens.push(FmtToken::LineComment(comment));
        self.width += comment.len() + 4;
    }

    fn add_comment(
        &mut self,
        src: &'src str,
        until: usize,
        mut sep: impl FnMut(&mut Self),
    ) -> Result<()> {
        let range = self.cur_offset..until;
        let comment = src
            .get(range.clone())
            .with_context(|| format!("span {range:?} is out of bounds for the source"))?;
        self.cur_offset = until;

        let mut comment_added = false;
        for comment in CommentParser(comment) {
            match comment {
                Comment::Line(line) => self.add_line_comment(line),
                Comment::Multi(inner) => self.add_raw_text(inner),
            }
            if replace(&mut comment_added, true) {
                sep(self);
            }
        }

        Ok(
            if comment_added && self.spacing.map_or(true, |s| s.before) {
                sep(self);
            },
        )
    }

    pub fn add_space(&mut self, ctx: &FormatCtx<'_, 'src>, at: LineColumn) -> Result<()> {
        self.add_raw_space();
        self.add_comment(ctx.input, ctx.pos_to_byte_offset(at)?, Self::add_raw_space)
    }

    fn add_text(
        &mut self,
        text: &'src str,
        ctx: &FormatCtx<'_, 'src>,
        at: LineColumn,
    ) -> Result<()> {
        self.add_comment(ctx.input, ctx.pos_to_byte_offset(at)?, Self::add_raw_space)?;
        self.add_raw_text(text);
        Ok(self.cur_offset += text.len())
    }

    fn add_raw_sep(&mut self) {
        self.tokens.push(FmtToken::Sep);
        self.width += self.spacing.map_or(false, |s| s.between) as usize;
    }

    pub fn add_sep(&mut self, ctx: &FormatCtx<'_, 'src>, at: LineColumn) -> Result<()> {
        self.add_raw_sep();
        self.add_comment(ctx.input, ctx.pos_to_byte_offset(at)?, Self::add_raw_sep)
    }

    /// adds a block and gives a mutable reference to it to `f`
    pub fn add_block<R>(
        &mut self,
        spacing: Option<Spacing>,
        chaining: ChainingRule,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if spacing.is_none() {
            self.spacing = None;
        }
        let mut block = Self::new(self.tokens.bump(), spacing, chaining, self.cur_offset);
        let res = f(&mut block);
        if matches!(block.tokens.last(), Some(FmtToken::Sep)) {
            block.tokens.pop();
        }
        self.width += block.width;
        self.cur_offset = block.cur_offset;
        self.tokens.push(FmtToken::Block(block));
        res
    }

    pub fn add_source(&mut self, ctx: &FormatCtx<'_, 'src>, loc: Location) -> Result<()> {
        let text = ctx
            .source_code(loc)
            .context("failed to get a token's source code")?;
        self.add_text(text, ctx, loc.start)
    }

    pub fn add_source_iter(
        &mut self,
        ctx: &FormatCtx<'_, 'src>,
        iter: impl IntoIterator<Item = impl Located>,
    ) -> Result<()> {
        Ok(for obj in iter {
            self.add_source(ctx, obj.loc())?;
        })
    }

    pub fn add_source_punctuated(
        &mut self,
        ctx: &FormatCtx<'_, 'src>,
        iter: &Punctuated<impl Located, impl Located>,
    ) -> Result<()> {
        Ok(for pair in iter.pairs() {
            self.add_source(ctx, pair.value().loc())?;
            if let Some(punct) = pair.punct() {
                self.add_source(ctx, punct.loc())?;
            }
        })
    }

    fn force_breaking(&mut self, ctx: &FormatCtx<'_, 'src>, offset: usize, indent: usize) {
        self.spacing = None;
        self.width = 0;
        let indent = indent + ctx.config.tab_spaces;
        let mut chain_intermediate_widths = Vec::new_in(ctx.alloc);
        let mut chain_broken = false;
        let mut tokens_iter = self.tokens.iter_with_prev_mut();
        while let Some((token, prev_tokens)) = tokens_iter.next() {
            match token {
                FmtToken::Text(text) => {
                    if let Some(len) = text.last_line_len() {
                        self.width = len;
                    } else {
                        self.width += text.len();
                    }
                }
                FmtToken::LineComment(comment) => self.width += comment.len() + 4,
                FmtToken::Sep => self.width = 0,
                FmtToken::Block(block) => {
                    if chain_broken {
                        block.force_breaking(ctx, offset + self.width, indent)
                    } else {
                        chain_broken = block.determine_breaking(ctx, offset + self.width, indent);
                    }

                    if chain_broken {
                        for (block, intermediate_width) in prev_tokens
                            .iter_mut()
                            .rev()
                            .filter_map(|t| map!(t, FmtToken::Block(b) => b))
                            .zip(chain_intermediate_widths.drain(..).rev())
                        {
                            block.force_breaking(ctx, offset + intermediate_width, indent)
                        }
                    } else {
                        match block.chaining_rule {
                            ChainingRule::Off => (),
                            ChainingRule::End => {
                                chain_intermediate_widths.clear();
                                chain_broken = false
                            }
                            ChainingRule::On => chain_intermediate_widths.push(self.width),
                        }
                    }
                    self.width += block.width;
                }
            }
        }
    }

    /// the returned boolean indicates whether the block was broken or not
    fn determine_breaking(
        &mut self,
        ctx: &FormatCtx<'_, 'src>,
        offset: usize,
        indent: usize,
    ) -> bool {
        let Some(spacing) = self.spacing else {
            self.force_breaking(ctx, offset, indent);
            return true;
        };

        let max_width =
            offset + indent + self.width + (spacing.around() && !self.tokens.is_empty()) as usize;
        (max_width >= ctx.config.yew.html_width)
            .on_true(|| self.force_breaking(ctx, offset, indent))
    }

    fn print(&self, indent: usize, cfg: &Config, out: &mut String) {
        #[derive(Clone, Copy)]
        enum Sep {
            None,
            Space,
            Newline,
        }

        let space_if = |c| if c { Sep::Space } else { Sep::None };

        fn print_token(token: &FmtToken, indent: usize, sep: Sep, cfg: &Config, out: &mut String) {
            match token {
                FmtToken::Text(text) => out.push_str(text),
                FmtToken::LineComment(comment) => {
                    if let Sep::Newline = sep {
                        out.push_str("//");
                        out.push_str(comment);
                        print_break(out, indent)
                    } else {
                        out.push_str("/*");
                        out.push_str(comment);
                        out.push_str("*/")
                    }
                }
                FmtToken::Sep => match sep {
                    Sep::None => (),
                    Sep::Space => out.push(' '),
                    Sep::Newline => print_break(out, indent),
                },
                FmtToken::Block(block) => block.print(indent, cfg, out),
            }
        }

        if self.tokens.is_empty() {
            if self.spacing.map_or(false, |s| s.around()) {
                out.push(' ');
            }
        } else if let Some(spacing) = self.spacing {
            if spacing.before {
                out.push(' ');
            }
            for token in &self.tokens {
                print_token(token, indent, space_if(spacing.between), cfg, out);
            }
            if spacing.after {
                out.push(' ');
            }
        } else {
            let new_indent = indent + cfg.tab_spaces;
            print_break(out, new_indent);
            for token in &self.tokens {
                print_token(token, new_indent, Sep::Newline, cfg, out);
            }
            print_break(out, indent);
        }
    }
}

pub struct FormatCtx<'fmt, 'src> {
    pub config: &'fmt Config,
    /// buffer for tokens stored in `FmtBlock`s
    alloc: &'fmt Bump,
    /// for error reporting purposes
    filename: &'src str,
    /// maps line number to byte offset in `input`
    offsets: &'fmt mut StdVec<usize>,
    /// the formatted code
    output: &'fmt mut String,
    /// the source code
    input: &'src str,
    /// to return errors from within AST traversal
    err: Result<Option<Diagnostic<()>>>,
    /// the end of `output` represented as byte offset into `input`
    cur_offset: usize,
    /// the end of `output` represented as position in `input`
    cur_pos: LineColumn,
}

impl<'fmt, 'src: 'fmt> Visit<'_> for FormatCtx<'fmt, 'src> {
    fn visit_item(&mut self, i: &'_ Item) {
        let attrs = match i {
            Item::Const(x) => &x.attrs,
            Item::Enum(x) => &x.attrs,
            Item::ExternCrate(x) => &x.attrs,
            Item::Fn(x) => &x.attrs,
            Item::ForeignMod(x) => &x.attrs,
            Item::Impl(x) => &x.attrs,
            Item::Macro(x) => &x.attrs,
            Item::Mod(x) => &x.attrs,
            Item::Static(x) => &x.attrs,
            Item::Struct(x) => &x.attrs,
            Item::Trait(x) => &x.attrs,
            Item::TraitAlias(x) => &x.attrs,
            Item::Type(x) => &x.attrs,
            Item::Union(x) => &x.attrs,
            Item::Use(x) => &x.attrs,
            _ => return,
        };
        if !is_skipped(attrs) {
            syn::visit::visit_item(self, i)
        }
    }

    fn visit_stmt(&mut self, i: &'_ Stmt) {
        let attrs = match i {
            Stmt::Local(x) => &x.attrs,
            Stmt::Macro(x) => &x.attrs,
            Stmt::Item(i) => return syn::visit::visit_item(self, i),
            _ => return,
        };
        if !is_skipped(attrs) {
            syn::visit::visit_stmt(self, i);
        }
    }

    // TODO: rewrite with a `try` block when those get stabilised
    fn visit_macro(&mut self, i: &Macro) {
        self.err = (|| -> Result<Option<Diagnostic<()>>> {
            let Some(name) = i.path.segments.last() else {
                return Ok(None);
            };
            if name.ident != "html" && name.ident != "html_nested" {
                return Ok(None);
            }

            let span = i.delimiter.span();
            let (opening_span, closing_span) = (span.open(), span.close());
            self.print_source(opening_span.start())?;

            let html_start = opening_span.end();
            if i.tokens.is_empty() {
                self.print_text("{", html_start)?;
                self.print_text("}", closing_span.end())?;
                return Ok(None);
            }

            let html = match syn::parse2::<Html>(i.tokens.clone()) {
                Ok(html) => html,
                Err(e) => {
                    let span = e.span();
                    let start = self.pos_to_byte_offset(span.start())?;
                    let end = self.pos_to_byte_offset(span.end())?;
                    return Ok(Some(
                        Diagnostic::error()
                            .with_message(e.to_string())
                            .with_labels(vec![Label::primary((), start..end)]),
                    ));
                }
            };
            let mut block = FmtBlock::new(
                self.alloc,
                Some(BLOCK_CHILDREN_SPACING),
                ChainingRule::Off,
                self.pos_to_byte_offset(html_start)?,
            );
            html.format(&mut block, self)?;

            self.print_text("{", html_start)?;
            self.print_fmt_block(block, closing_span.start())?;
            self.print_text("}", closing_span.end())?;
            Ok(None)
        })();
    }
}

impl Formatter {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            tokens_buf: Bump::new(),
            offsets: vec![],
            output: String::new(),
        }
    }

    pub fn format<'fmt, 'src: 'fmt>(
        &'fmt mut self,
        filename: &'src str,
        input: &'src str,
    ) -> Result<FormatResult<'fmt, 'src>> {
        self.output.clear();
        self.offsets.clear();
        self.tokens_buf.reset();
        let mut ctx = FormatCtx {
            alloc: &self.tokens_buf,
            config: &self.config,
            offsets: &mut self.offsets,
            output: &mut self.output,
            filename,
            input,
            err: Ok(None),
            cur_offset: 0,
            cur_pos: LineColumn { line: 1, column: 0 },
        };
        let file = syn::parse_file(input)?;
        ctx.offsets.push(0);
        ctx.offsets.extend(
            input
                .char_indices()
                .filter_map(|(i, c)| (c == '\n').then_some(i + 1)),
        );

        ctx.visit_file(&file);
        ctx.finalise()
    }
}

impl<'fmt, 'src> FormatCtx<'fmt, 'src> {
    fn pos_to_byte_offset(&self, LineColumn { line, column }: LineColumn) -> Result<usize> {
        let line_start = *self
            .offsets
            .get(line.saturating_sub(1))
            .with_context(|| format!("line {line} doesn't exist in the source file"))?;
        let column = self.input[line_start..]
            .chars()
            .take(column)
            .map(char::len_utf8)
            .sum();

        line_start.checked_add(column).with_context(|| {
            format!("source position {line}:{column} can't be converted to a byte offset")
        })
    }

    pub fn source_code(&self, loc: Location) -> Result<&'src str> {
        let start = self
            .pos_to_byte_offset(loc.start)
            .context("failed to find the start of the span")?;
        let end = self
            .pos_to_byte_offset(loc.end)
            .context("failed to find the end of the span")?;
        self.input
            .get(start..end)
            .with_context(|| format!("byte range {start}..{end} is invalid for the source code"))
    }

    fn line_indent(&self, line: usize) -> Result<usize> {
        enum State {
            Space,
            CommentStart,
            Comment,
            CommentEnd,
        }
        let mut state = State::Space;

        let &start = self
            .offsets
            .get(line - 1)
            .with_context(|| format!("line {line} doesn't exist in the source file"))?;
        let line = unsafe { self.input.get_unchecked(start..) };
        for (i, ch) in line.char_indices() {
            match ch {
                ' ' => {
                    state = match state {
                        State::Space => continue,
                        State::CommentStart => State::Space,
                        State::Comment => continue,
                        State::CommentEnd => State::Space,
                    }
                }
                '/' => {
                    state = match state {
                        State::Space => State::CommentStart,
                        State::CommentStart => bail!("line {line} of the source file is empty"),
                        State::Comment => continue,
                        State::CommentEnd => State::Space,
                    }
                }
                '*' => {
                    state = match state {
                        State::Space => continue,
                        State::CommentStart => State::Comment,
                        State::Comment => State::CommentEnd,
                        State::CommentEnd => continue,
                    }
                }
                '\n' => bail!("line {line} of the source file is empty"),
                _ => match state {
                    State::Space => return Ok(i),
                    State::CommentStart => return Ok(i - 1),
                    State::Comment => continue,
                    State::CommentEnd => continue,
                },
            }
        }
        bail!("line {line} of the source file is empty")
    }

    fn print_source(&mut self, until: LineColumn) -> Result<()> {
        let until_byte = self.pos_to_byte_offset(until)?;
        let from = self.cur_offset;
        let new = self.input.get(from..until_byte).with_context(|| {
            format!("range {from} .. {until_byte} is out of bounds for the source file")
        })?;
        self.cur_offset = until_byte;
        self.cur_pos = until;
        Ok(self.output.push_str(new))
    }

    // `end` is the position in the source file asssumed to be the end of the text
    fn print_text(&mut self, text: &str, end: LineColumn) -> Result<()> {
        self.output.push_str(text);
        self.cur_pos = end;
        let off = self.pos_to_byte_offset(end)?;
        Ok(self.cur_offset = off)
    }

    // `end` is the position in the source file asssumed to be the end of the formatted sequence
    fn print_fmt_block(&mut self, mut block: FmtBlock<'fmt, 'src>, end: LineColumn) -> Result<()> {
        let indent = self.line_indent(self.cur_pos.line)?;
        block.determine_breaking(self, self.cur_pos.column - indent, indent);
        block.print(indent, self.config, self.output);
        self.cur_pos = end;
        let off = self.pos_to_byte_offset(end)?;
        Ok(self.cur_offset = off)
    }

    fn finalise(self) -> Result<FormatResult<'fmt, 'src>> {
        let rest = unsafe { self.input.get_unchecked(self.cur_offset..) };
        self.output.push_str(rest);
        let new_len = self.output.trim_end().len();
        self.output.truncate(new_len);
        self.output.push('\n');
        self.err.map(|diagnostic| FormatResult {
            filename: self.filename,
            source: self.input,
            output: match diagnostic {
                Some(diagnostic) => Err(diagnostic),
                None => Ok(self.output.as_str()),
            },
        })
    }
}

pub struct FormatResult<'fmt, 'src> {
    filename: &'src str,
    source: &'src str,
    output: Result<&'fmt str, Diagnostic<()>>,
}

impl<'fmt, 'src> FormatResult<'fmt, 'src> {
    /// if the result is an error, write it into stderr, if it's successfully formatted code,
    /// return it
    pub fn emit_error(self, writer: &mut dyn WriteColor) -> Result<Option<&'fmt str>> {
        let diagnostic = match self.output {
            Ok(out) => return Ok(Some(out)),
            Err(x) => x,
        };
        term::emit(
            writer,
            &term::Config::default(),
            &SimpleFile::new(self.filename, self.source),
            &diagnostic,
        )?;
        Ok(None)
    }
}

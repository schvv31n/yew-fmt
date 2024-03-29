use crate::{
    config::UseSmallHeuristics,
    formatter::{ChainingRule, FmtBlock, Format, FormatCtx, Located, Location, Spacing},
    utils::{
        default, OptionExt, ParseBufferExt, ParseWithCtx, PunctuatedExt, Result, TokenIter,
        TokenTreeExt,
    },
};
use anyhow::Context;
use proc_macro2::{Delimiter, LineColumn, TokenStream, TokenTree};
use quote::ToTokens;
use std::{iter::from_fn, ops::Deref};
use syn::{
    braced,
    buffer::Cursor,
    ext::IdentExt,
    parse::{Parse, ParseStream},
    parse2,
    punctuated::{Pair, Punctuated},
    spanned::Spanned,
    token::Brace,
    Block, Expr, Ident, Lit, Local, LocalInit, Pat, PatType, Stmt, Token, Type,
};

/// Overrides `Ident`'s default `Parse` behaviour by accepting Rust keywords
pub struct AnyIdent(Ident);

impl Parse for AnyIdent {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ident::parse_any(input).map(Self)
    }
}

impl ToTokens for AnyIdent {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        self.0.to_tokens(tokens)
    }
}

impl Deref for AnyIdent {
    type Target = Ident;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub enum Html {
    Tree(HtmlTree),
    Value(Box<HtmlBlockContent>),
}

pub enum HtmlTreeKind {
    Element,
    Block,
    If,
    For,
    Match,
}

pub enum HtmlTree {
    Element(Box<HtmlElement>),
    Block(Box<HtmlBlock>),
    If(Box<HtmlIf>),
    For(Box<HtmlFor>),
    Match(Box<HtmlMatch>),
    Let(Box<HtmlLet>),
}

pub struct HtmlBlock {
    pub brace: Brace,
    pub content: HtmlBlockContent,
}

pub enum HtmlBlockContent {
    Expr(Expr),
    Iterable(Token![for], Expr),
}

pub enum HtmlElement {
    Fragment(HtmlFragment),
    Dynamic(HtmlDynamicElement),
    Literal(HtmlLiteralElement),
}

pub struct HtmlFragment {
    pub lt_token: Token![<],
    pub key: Option<HtmlProp>,
    pub gt_token: Token![>],
    pub children: Vec<HtmlTree>,
    pub closing_lt_token: Token![<],
    pub div_token: Token![/],
    pub closing_gt_token: Token![>],
}

pub struct HtmlDynamicElement {
    pub lt_token: Token![<],
    pub at_token: Token![@],
    pub name: Block,
    pub props: Vec<HtmlProp>,
    pub children: Vec<HtmlTree>,
    pub closing_tag: Option<(Token![>], Token![<], Token![@])>,
    pub div_token: Token![/],
    pub closing_gt_token: Token![>],
}

pub struct HtmlLiteralElement {
    pub lt_token: Token![<],
    pub name: TokenStream,
    pub props: Vec<HtmlProp>,
    pub prop_base: Option<(Token![..], Expr)>,
    pub children: Vec<HtmlTree>,
    pub closing_tag: Option<(Token![>], Token![<], TokenStream)>,
    pub div_token: Token![/],
    pub closing_gt_token: Token![>],
}

pub struct HtmlProp {
    pub access_spec: Option<Token![~]>,
    pub kind: HtmlPropKind,
}

pub enum HtmlPropKind {
    Shortcut(Brace, Punctuated<AnyIdent, Token![-]>),
    Literal(Punctuated<AnyIdent, Token![-]>, Token![=], Lit),
    Block(Punctuated<AnyIdent, Token![-]>, Token![=], Block),
}

pub struct HtmlIf {
    pub if_token: Token![if],
    pub condition: Expr,
    pub brace: Brace,
    pub then_branch: Vec<HtmlTree>,
    pub else_branch: Option<HtmlElse>,
}

pub enum HtmlElse {
    If(Token![else], Box<HtmlIf>),
    Tree(Token![else], Brace, Vec<HtmlTree>),
}

pub struct HtmlFor {
    pub for_token: Token![for],
    pub pat: Pat,
    pub in_token: Token![in],
    pub iter: Expr,
    pub brace: Brace,
    pub body: Vec<HtmlTree>,
}

pub struct HtmlMatch {
    pub match_token: Token![match],
    pub expr: Expr,
    pub brace: Brace,
    pub arms: Punctuated<HtmlMatchArm, Token![,]>,
}

pub struct HtmlMatchArm {
    pub pat: Pat,
    pub guard: Option<(Token![if], Box<Expr>)>,
    pub fat_arrow_token: Token![=>],
    pub body: Html,
}

pub struct HtmlLet(Local);

impl ParseWithCtx for Html {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        /// Assumes that `cursor` is pointing at a `for` token
        fn for_loop_like(cursor: Cursor<'_>) -> bool {
            let Some((_, cursor)) = cursor.token_tree() else { return false };
            // The `take_while` call makes sure that e.g. `html!(for for i in 0 .. 10 {})` is
            // classified correctly
            TokenIter(cursor).take_while(|t| !t.is_ident("for")).any(|t| t.is_ident("in"))
        }

        let cursor = input.cursor();
        Ok(match HtmlTree::peek_html_type(cursor) {
            Some(HtmlTreeKind::For) => {
                if ext && for_loop_like(cursor) {
                    Self::Tree(HtmlTree::For(input.parse_with_ctx(ext)?))
                } else {
                    Self::Value(HtmlBlockContent::Iterable(input.parse()?, input.parse()?).into())
                }
            }
            Some(HtmlTreeKind::Match) => {
                if ext {
                    Self::Tree(HtmlTree::Match(input.parse_with_ctx(ext)?))
                } else {
                    Self::Value(HtmlBlockContent::Expr(input.parse()?).into())
                }
            }
            Some(HtmlTreeKind::If) => Self::Tree(HtmlTree::If(input.parse_with_ctx(ext)?)),
            Some(HtmlTreeKind::Block) => Self::Tree(HtmlTree::Block(input.parse()?)),
            Some(HtmlTreeKind::Element) => {
                Self::Tree(HtmlTree::Element(input.parse_with_ctx(ext)?))
            }
            None => Self::Value(input.parse()?),
        })
    }
}

impl ParseWithCtx for HtmlTree {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let cursor = input.cursor();
        Ok(if HtmlIf::parseable(cursor) {
            Self::If(input.parse_with_ctx(ext)?)
        } else if HtmlElement::parseable(cursor) {
            Self::Element(input.parse_with_ctx(ext)?)
        } else if ext && HtmlFor::parseable(cursor) {
            Self::For(input.parse_with_ctx(ext)?)
        } else if ext && HtmlMatch::parseable(cursor) {
            Self::Match(input.parse_with_ctx(ext)?)
        } else if ext && HtmlLet::parseable(cursor) {
            Self::Let(input.parse()?)
        } else {
            Self::Block(input.parse()?)
        })
    }
}

impl HtmlBlock {
    fn parseable(cursor: Cursor) -> bool {
        cursor.group(Delimiter::Brace).is_some()
    }
}

impl Parse for HtmlBlock {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        let brace = braced!(content in input);
        HtmlBlockContent::parse(&content).map(|content| Self { content, brace })
    }
}

impl Parse for HtmlBlockContent {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(if let Ok(for_token) = input.parse() {
            Self::Iterable(for_token, input.parse()?)
        } else {
            Self::Expr(input.parse()?)
        })
    }
}

impl HtmlElement {
    fn parseable(cursor: Cursor) -> bool {
        cursor.punct().is_some_and(|(p, _)| p.as_char() == '<')
    }
}

impl ParseWithCtx for HtmlElement {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        Ok(if HtmlFragment::parseable(input.cursor()) {
            Self::Fragment(input.parse_with_ctx(ext)?)
        } else if HtmlDynamicElement::parseable(input.cursor()) {
            Self::Dynamic(input.parse_with_ctx(ext)?)
        } else {
            Self::Literal(input.parse_with_ctx(ext)?)
        })
    }
}

impl HtmlFragment {
    fn parseable(cursor: Cursor) -> bool {
        let Some((_, cursor)) = cursor.punct().filter(|(p, _)| p.as_char() == '<') else {
            return false;
        };
        match cursor.token_tree() {
            Some((TokenTree::Punct(p), _)) if p.as_char() == '>' => true, // <>...
            Some((_, c)) => c.punct().is_some_and(|(p, _)| p.as_char() == '='), // <key=...
            None => false,
        }
    }
}

impl ParseWithCtx for HtmlFragment {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let lt_token = input.parse()?;
        let (key, gt_token) = if input.peek(Token![>]) {
            (None, input.parse()?)
        } else {
            (Some(input.parse()?), input.parse()?)
        };

        Ok(Self {
            lt_token,
            key,
            gt_token,
            children: HtmlTree::parse_children(input, ext)?,
            closing_lt_token: input.parse()?,
            div_token: input.parse()?,
            closing_gt_token: input.parse()?,
        })
    }
}

impl ParseWithCtx for HtmlDynamicElement {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let lt_token = input.parse()?;
        let at_token = input.parse()?;
        let name = input.parse()?;

        let mut props = vec![];
        while input.cursor().punct().is_none() {
            props.push(input.parse()?)
        }

        let (children, closing_tag, div_token) = if input.peek(Token![>]) {
            let gt_token = input.parse()?;
            let children = HtmlTree::parse_children(input, ext)?;
            let closing_lt_token = input.parse()?;
            let div_token = input.parse()?;
            let closing_at_token = input.parse()?;
            (children, Some((gt_token, closing_lt_token, closing_at_token)), div_token)
        } else {
            (vec![], None, input.parse()?)
        };

        Ok(Self {
            lt_token,
            at_token,
            name,
            props,
            children,
            closing_tag,
            div_token,
            closing_gt_token: input.parse()?,
        })
    }
}

impl ParseWithCtx for HtmlLiteralElement {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        fn prop_base_collector(input: ParseStream) -> impl Iterator<Item = TokenTree> + '_ {
            from_fn(move || {
                (!input.peek(Token![>]) && !input.peek(Token![/])).then(|| input.parse().ok())?
            })
        }

        fn get_name(input: ParseStream) -> syn::Result<TokenStream> {
            Ok(if let Ok(ty) = Type::parse(input) {
                ty.into_token_stream()
            } else {
                let name = Punctuated::<AnyIdent, Token![-]>::parse_separated_nonempty(input)?;
                name.into_token_stream()
            })
        }

        let lt_token = input.parse()?;
        let name = get_name(input)?;

        let mut props = vec![];
        while input.cursor().punct().is_none() {
            props.push(input.parse()?)
        }
        let prop_base = if input.peek(Token![..]) {
            Some((input.parse()?, parse2(prop_base_collector(input).collect())?))
        } else {
            None
        };

        let (children, closing_tag, div_token) = if input.peek(Token![>]) {
            let gt_token = input.parse()?;
            let children = HtmlTree::parse_children(input, ext)?;
            let closing_lt_token = input.parse()?;
            let div_token = input.parse()?;
            let closing_name = get_name(input)?;
            (children, Some((gt_token, closing_lt_token, closing_name)), div_token)
        } else {
            (vec![], None, input.parse()?)
        };

        Ok(Self {
            lt_token,
            name,
            props,
            prop_base,
            children,
            closing_tag,
            div_token,
            closing_gt_token: input.parse()?,
        })
    }
}

impl Parse for HtmlProp {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let access_spec = input.parse().ok();
        let kind = if input.peek(Brace) {
            let inner;
            let brace = braced!(inner in input);
            HtmlPropKind::Shortcut(brace, Punctuated::parse_terminated(&inner)?)
        } else {
            let name = Punctuated::parse_separated_nonempty(input)?;
            let eq_token = input.parse()?;
            if input.peek(Brace) {
                HtmlPropKind::Block(name, eq_token, input.parse()?)
            } else {
                HtmlPropKind::Literal(name, eq_token, input.parse()?)
            }
        };
        Ok(Self { access_spec, kind })
    }
}

impl ParseWithCtx for HtmlIf {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let then_block;
        Ok(Self {
            if_token: input.parse()?,
            condition: Expr::parse_without_eager_brace(input)?,
            brace: braced!(then_block in input),
            then_branch: HtmlTree::parse_children(&then_block, ext)?,
            else_branch: HtmlElse::parseable(input.cursor())
                .then(|| input.parse_with_ctx(ext))
                .transpose()?,
        })
    }
}

impl ParseWithCtx for HtmlElse {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let else_token = input.parse()?;
        Ok(if HtmlIf::parseable(input.cursor()) {
            Self::If(else_token, input.parse_with_ctx(ext)?)
        } else {
            let inner;
            Self::Tree(else_token, braced!(inner in input), HtmlTree::parse_children(&inner, ext)?)
        })
    }
}

impl ParseWithCtx for HtmlFor {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let body;
        Ok(Self {
            for_token: input.parse()?,
            pat: Pat::parse_multi_with_leading_vert(input)?,
            in_token: input.parse()?,
            iter: Expr::parse_without_eager_brace(input)?,
            brace: braced!(body in input),
            body: HtmlTree::parse_children(&body, ext)?,
        })
    }
}

impl ParseWithCtx for HtmlMatch {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        let body;
        Ok(Self {
            match_token: input.parse()?,
            expr: Expr::parse_without_eager_brace(input)?,
            brace: braced!(body in input),
            arms: Punctuated::parse_terminated_with_ctx(&body, ext)?,
        })
    }
}

impl ParseWithCtx for HtmlMatchArm {
    type Context = bool;

    fn parse(input: ParseStream, ext: bool) -> syn::Result<Self> {
        Ok(Self {
            pat: Pat::parse_multi_with_leading_vert(input)?,
            guard: if let Ok(if_token) = input.parse() {
                Some((if_token, input.parse()?))
            } else {
                None
            },
            fat_arrow_token: input.parse()?,
            body: input.parse_with_ctx(ext)?,
        })
    }
}

impl Parse for HtmlLet {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let let_token = input.parse()?;

        let mut pat = Pat::parse_single(input)?;
        if let Some(colon_token) = input.parse()? {
            pat = Pat::Type(PatType {
                attrs: vec![],
                pat: Box::new(pat),
                colon_token,
                ty: input.parse()?,
            });
        }

        let init = if let Some(eq_token) = input.parse()? {
            Some(LocalInit {
                eq_token,
                expr: input.parse()?,
                diverge: Option::<Token![else]>::parse(input)?.try_zip(|| input.parse())?,
            })
        } else {
            None
        };

        Ok(Self(Local { attrs: vec![], let_token, pat, init, semi_token: input.parse()? }))
    }
}

impl HtmlTree {
    fn peek_html_type(cursor: Cursor) -> Option<HtmlTreeKind> {
        if HtmlIf::parseable(cursor) {
            Some(HtmlTreeKind::If)
        } else if HtmlFor::parseable(cursor) {
            Some(HtmlTreeKind::For)
        } else if HtmlMatch::parseable(cursor) {
            Some(HtmlTreeKind::Match)
        } else if HtmlElement::parseable(cursor) {
            Some(HtmlTreeKind::Element)
        } else if HtmlBlock::parseable(cursor) {
            Some(HtmlTreeKind::Block)
        } else {
            None
        }
    }

    fn parse_children(input: ParseStream, ext: bool) -> syn::Result<Vec<Self>> {
        let mut res = vec![];
        while !(input.is_empty() || input.peek(Token![<]) && input.peek2(Token![/])) {
            res.push(input.parse_with_ctx(ext)?)
        }
        Ok(res)
    }
}

impl HtmlDynamicElement {
    fn parseable(cursor: Cursor) -> bool {
        let Some((_, cursor)) = cursor.punct().filter(|(p, _)| p.as_char() == '<') else {
            return false;
        };
        cursor.punct().is_some_and(|(p, _)| p.as_char() == '@')
    }
}

impl HtmlIf {
    fn parseable(cursor: Cursor) -> bool {
        cursor.ident().is_some_and(|(i, _)| i == "if")
    }
}

impl HtmlElse {
    fn parseable(cursor: Cursor) -> bool {
        cursor.ident().is_some_and(|(i, _)| i == "else")
    }
}

impl HtmlFor {
    fn parseable(cursor: Cursor) -> bool {
        cursor.ident().is_some_and(|(i, _)| i == "for")
    }
}

impl HtmlMatch {
    fn parseable(cursor: Cursor) -> bool {
        cursor.ident().is_some_and(|(i, _)| i == "match")
    }
}

impl HtmlLet {
    fn parseable(cursor: Cursor) -> bool {
        cursor.ident().is_some_and(|(i, _)| i == "let")
    }
}

pub const fn props_spacing(self_closing: bool) -> Spacing {
    Spacing { before: true, between: true, after: self_closing }
}

pub fn element_children_spacing(ctx: &FormatCtx, children: &[HtmlTree]) -> Option<Spacing> {
    match ctx.config.yew.use_small_heuristics {
        UseSmallHeuristics::Off => None,
        UseSmallHeuristics::Default => {
            children.iter().all(|child| matches!(child, HtmlTree::Block(_))).then(default)
        }
        UseSmallHeuristics::Max => Some(default()),
    }
}

pub fn block_children_spacing(ctx: &FormatCtx) -> Option<Spacing> {
    (ctx.config.yew.use_small_heuristics == UseSmallHeuristics::Max).then_some(Spacing::AROUND)
}

impl<'src> Format<'src> for Html {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        match self {
            Html::Tree(tree) => tree.format(block, ctx),
            Html::Value(val) => val.format(block, ctx),
        }
    }
}

impl<'src> Format<'src> for HtmlTree {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        match self {
            HtmlTree::Element(e) => e.format(block, ctx),
            HtmlTree::Block(b) => b.format(block, ctx),
            HtmlTree::If(i) => i.format(block, ctx),
            HtmlTree::For(f) => f.format(block, ctx),
            HtmlTree::Match(m) => m.format(block, ctx),
            HtmlTree::Let(l) => l.format(block, ctx),
        }
    }
}

impl<'src> Format<'src> for HtmlElement {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        match self {
            HtmlElement::Fragment(f) => f.format(block, ctx),
            HtmlElement::Dynamic(d) => d.format(block, ctx),
            HtmlElement::Literal(l) => l.format(block, ctx),
        }
    }
}

impl<'src> Format<'src> for HtmlFragment {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.lt_token)?;
        if let Some(key) = &self.key {
            key.format(block, ctx)?;
        }

        block.add_delimited_block(
            ctx,
            self.gt_token,
            self.closing_lt_token,
            element_children_spacing(ctx, &self.children),
            ChainingRule::Off,
            |block, ctx| {
                for child in &self.children {
                    child.format(block, ctx)?;
                    block.add_sep(ctx, child.end())?;
                }
                Ok(())
            },
        )?;

        block.add_source(ctx, self.div_token)?;
        block.add_source(ctx, self.closing_gt_token)
    }
}

impl<'src> Format<'src> for HtmlDynamicElement {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.lt_token)?;
        block.add_source(ctx, self.at_token)?;
        self.name.format(block, ctx)?;
        let closing_tag = self
            .closing_tag
            .as_ref()
            .filter(|_| !self.children.is_empty() || !ctx.config.yew.self_close_elements);

        block.add_block(Some(props_spacing(closing_tag.is_none())), ChainingRule::On, |block| {
            for prop in &self.props {
                prop.format(block, ctx)?;
                block.add_sep(ctx, prop.end())?;
            }
            anyhow::Ok(())
        })?;

        if let Some((gt, closing_lt, closing_at)) = closing_tag {
            block.add_delimited_block(
                ctx,
                gt,
                closing_lt,
                element_children_spacing(ctx, &self.children),
                ChainingRule::End,
                |block, ctx| {
                    for child in &self.children {
                        child.format(block, ctx)?;
                        block.add_sep(ctx, child.end())?;
                    }
                    Ok(())
                },
            )?;
            block.add_source(ctx, self.div_token)?;
            block.add_source(ctx, closing_at)?;
            block.add_source(ctx, self.closing_gt_token)
        } else {
            block.add_source(ctx, self.div_token)?;
            block.add_source(ctx, self.closing_gt_token)
        }
    }
}

impl<'src> Format<'src> for HtmlLiteralElement {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.lt_token)?;
        block.add_source_iter(ctx, self.name.clone())?;
        let closing_tag = self
            .closing_tag
            .as_ref()
            .filter(|_| !self.children.is_empty() || !ctx.config.yew.self_close_elements);

        block.add_block(
            Some(props_spacing(closing_tag.is_none())),
            closing_tag.choose(ChainingRule::On, ChainingRule::Off),
            |block| {
                for prop in &self.props {
                    prop.format(block, ctx)?;
                    block.add_sep(ctx, prop.end())?;
                }
                if let Some((dotdot, prop_base)) = &self.prop_base {
                    block.add_source(ctx, dotdot)?;
                    block.add_source(ctx, prop_base)?;
                    block.add_sep(ctx, prop_base.end())?;
                }
                anyhow::Ok(())
            },
        )?;

        if let Some((gt, closing_lt, closing_name)) = closing_tag {
            block.add_delimited_block(
                ctx,
                gt,
                closing_lt,
                element_children_spacing(ctx, &self.children),
                ChainingRule::End,
                |block, ctx| {
                    Ok(for child in &self.children {
                        child.format(block, ctx)?;
                        block.add_sep(ctx, child.end())?;
                    })
                },
            )?;
            block.add_source(ctx, self.div_token)?;
            block.add_source_iter(ctx, closing_name.clone())?;
            block.add_source(ctx, self.closing_gt_token)
        } else {
            block.add_source(ctx, self.div_token)?;
            block.add_source(ctx, self.closing_gt_token)
        }
    }
}

impl<'src> Format<'src> for HtmlProp {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source_iter(ctx, self.access_spec)?;
        match &self.kind {
            HtmlPropKind::Shortcut(brace, name) => {
                block.add_source(ctx, brace.span.open())?;
                block.add_source_punctuated(ctx, name)?;
                block.add_source(ctx, brace.span.close())
            }
            HtmlPropKind::Literal(name, eq, lit) => {
                block.add_source_punctuated(ctx, name)?;
                block.add_source(ctx, eq)?;
                block.add_source(ctx, lit)
            }
            HtmlPropKind::Block(name, eq, expr) => match &*expr.stmts {
                [Stmt::Expr(Expr::Path(p), None)]
                    if ctx.config.yew.use_prop_init_shorthand
                        && name.len() == 1
                        && p.path.is_ident(&**name.first().context("prop name is empty")?) =>
                {
                    expr.format(block, ctx)
                }
                [Stmt::Expr(Expr::Lit(l), None)] if ctx.config.yew.unwrap_literal_prop_values => {
                    block.add_source_punctuated(ctx, name)?;
                    block.add_source(ctx, eq)?;
                    block.add_source(ctx, l)
                }
                _ => {
                    block.add_source_punctuated(ctx, name)?;
                    block.add_source(ctx, eq)?;
                    expr.format(block, ctx)
                }
            },
        }
    }
}

impl<'src> Format<'src> for Block {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.brace_token.span.open())?;
        match &*self.stmts {
            [] => (),
            [only] => block.add_source(ctx, only)?,
            [first, .., last] => {
                block.add_source(ctx, Location { start: first.start(), end: last.end() })?
            }
        }
        block.add_source(ctx, self.brace_token.span.close())
    }
}

impl<'src> Format<'src> for HtmlBlock {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.brace.span.open())?;
        self.content.format_with_space(block, ctx)?;
        block.add_source_with_space(ctx, self.brace.span.close())
    }
}

impl<'src> Format<'src> for HtmlBlockContent {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        match self {
            Self::Expr(e) => block.add_source(ctx, e),
            Self::Iterable(r#for, e) => {
                block.add_source(ctx, r#for)?;
                block.add_source_with_space(ctx, e)
            }
        }
    }
}

impl<'src> Format<'src> for HtmlIf {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.if_token.loc())?;
        block.add_source_with_space(ctx, &self.condition)?;
        block.add_delimited_block_with_space(
            ctx,
            self.brace.span.open(),
            self.brace.span.close(),
            block_children_spacing(ctx),
            self.else_branch.choose(ChainingRule::On, ChainingRule::End),
            |block, ctx| {
                for child in &self.then_branch {
                    child.format(block, ctx)?;
                    block.add_sep(ctx, child.end())?;
                }
                Ok(())
            },
        )?;
        self.else_branch.as_ref().try_map_or((), |b| b.format_with_space(block, ctx))
    }
}

impl<'src> Format<'src> for HtmlElse {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        match self {
            Self::If(r#else, r#if) => {
                block.add_source(ctx, r#else)?;
                r#if.format_with_space(block, ctx)
            }
            Self::Tree(r#else, brace, children) => {
                block.add_source(ctx, r#else)?;
                block.add_delimited_block_with_space(
                    ctx,
                    brace.span.open(),
                    brace.span.close(),
                    block_children_spacing(ctx),
                    ChainingRule::End,
                    |block, ctx| {
                        for child in children {
                            child.format(block, ctx)?;
                            block.add_sep(ctx, child.end())?;
                        }
                        Ok(())
                    },
                )
            }
        }
    }
}

impl<'src> Format<'src> for HtmlFor {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.for_token)?;
        block.add_source_with_space(ctx, &self.pat)?;
        block.add_source_with_space(ctx, self.in_token)?;
        block.add_source_with_space(ctx, &self.iter)?;
        block.add_delimited_block_with_space(
            ctx,
            self.brace.span.open(),
            self.brace.span.close(),
            block_children_spacing(ctx),
            ChainingRule::Off,
            |block, ctx| {
                for child in &self.body {
                    child.format(block, ctx)?;
                    block.add_sep(ctx, child.end())?;
                }
                Ok(())
            },
        )
    }
}

impl<'src> Format<'src> for HtmlMatch {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, self.match_token)?;
        block.add_source_with_space(ctx, &self.expr)?;
        block.add_delimited_block_with_space(
            ctx,
            self.brace.span.open(),
            self.brace.span.close(),
            block_children_spacing(ctx).map(|s| Spacing { between: true, ..s }),
            ChainingRule::Off,
            |block, ctx| {
                for (arm, comma) in self.arms.pairs().map(Pair::into_tuple) {
                    arm.format(block, ctx)?;
                    let sep_at = if let Some(comma) = comma {
                        block.add_source(ctx, comma)?;
                        comma.end()
                    } else {
                        let at = arm.end();
                        block.add_text(ctx, ",", at)?;
                        LineColumn { line: at.line, column: at.column + 1 }
                    };
                    block.add_aware_sep(ctx, sep_at, 2)?;
                }
                Ok(())
            },
        )
    }
}

impl<'src> Format<'src> for HtmlMatchArm {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        block.add_source(ctx, &self.pat)?;
        if let Some((if_token, guard)) = &self.guard {
            block.add_source_with_space(ctx, if_token)?;
            block.add_source_with_space(ctx, guard)?;
        }
        block.add_source_with_space(ctx, self.fat_arrow_token)?;
        self.body.format_with_space(block, ctx)
    }
}

impl<'src> Format<'src> for HtmlLet {
    fn format(&self, block: &mut FmtBlock<'_, 'src>, ctx: &mut FormatCtx<'_, 'src>) -> Result {
        let Self(stmt) = self;
        block.add_source(ctx, stmt.let_token)?;
        if let Pat::Type(PatType { pat, colon_token, ty, .. }) = &stmt.pat {
            block.add_source_with_space(ctx, pat)?;
            block.add_source_with_space(ctx, colon_token)?;
            block.add_source_with_space(ctx, ty)?;
        } else {
            block.add_source_with_space(ctx, &stmt.pat)?;
        };
        if let Some(init) = &stmt.init {
            block.add_source_with_space(ctx, init.eq_token)?;
            block.add_source_with_space(ctx, &init.expr)?;
            if let Some((else_token, expr)) = &init.diverge {
                block.add_source_with_space(ctx, else_token)?;
                block.add_source_with_space(ctx, expr)?;
            }
        }
        block.add_source(ctx, stmt.semi_token)
    }
}

impl Located for Html {
    fn start(&self) -> LineColumn {
        match self {
            Self::Tree(tree) => tree.start(),
            Self::Value(val) => val.start(),
        }
    }

    fn end(&self) -> LineColumn {
        match self {
            Self::Tree(tree) => tree.end(),
            Self::Value(val) => val.end(),
        }
    }

    fn loc(&self) -> Location {
        match self {
            Self::Tree(tree) => tree.loc(),
            Self::Value(val) => val.loc(),
        }
    }
}

impl Located for HtmlTree {
    fn start(&self) -> LineColumn {
        match self {
            Self::Element(x) => x.start(),
            Self::Block(x) => x.start(),
            Self::If(x) => x.start(),
            Self::For(x) => x.start(),
            Self::Match(x) => x.start(),
            Self::Let(x) => x.start(),
        }
    }

    fn end(&self) -> LineColumn {
        match self {
            Self::Element(x) => x.end(),
            Self::Block(x) => x.end(),
            Self::If(x) => x.end(),
            Self::For(x) => x.end(),
            Self::Match(x) => x.end(),
            Self::Let(x) => x.end(),
        }
    }

    fn loc(&self) -> Location {
        match self {
            Self::Element(x) => x.loc(),
            Self::Block(x) => x.loc(),
            Self::If(x) => x.loc(),
            Self::For(x) => x.loc(),
            Self::Match(x) => x.loc(),
            Self::Let(x) => x.loc(),
        }
    }
}

impl Located for HtmlElement {
    fn start(&self) -> LineColumn {
        match self {
            Self::Fragment(x) => x.start(),
            Self::Dynamic(x) => x.start(),
            Self::Literal(x) => x.start(),
        }
    }

    fn end(&self) -> LineColumn {
        match self {
            Self::Fragment(x) => x.end(),
            Self::Dynamic(x) => x.end(),
            Self::Literal(x) => x.end(),
        }
    }

    fn loc(&self) -> Location {
        match self {
            Self::Fragment(x) => x.loc(),
            Self::Dynamic(x) => x.loc(),
            Self::Literal(x) => x.loc(),
        }
    }
}

impl Located for HtmlFragment {
    fn start(&self) -> LineColumn {
        self.lt_token.start()
    }
    fn end(&self) -> LineColumn {
        self.closing_gt_token.end()
    }
}

impl Located for HtmlDynamicElement {
    fn start(&self) -> LineColumn {
        self.lt_token.start()
    }

    fn end(&self) -> LineColumn {
        self.closing_gt_token.end()
    }
}

impl Located for HtmlLiteralElement {
    fn start(&self) -> LineColumn {
        self.lt_token.start()
    }

    fn end(&self) -> LineColumn {
        self.closing_gt_token.end()
    }
}

impl Located for HtmlProp {
    fn start(&self) -> LineColumn {
        if let Some(tilde) = &self.access_spec {
            return tilde.span.start();
        }

        match &self.kind {
            HtmlPropKind::Shortcut(brace, _) => brace.span.open().start(),
            HtmlPropKind::Literal(name, ..) | HtmlPropKind::Block(name, ..) => unsafe {
                // Safety: the name of the prop is guaranteed to be non-empty by
                // `Punctuated::parse_terminated(_nonempty)`
                name.first().unwrap_unchecked().span().start()
            },
        }
    }

    fn end(&self) -> LineColumn {
        match &self.kind {
            HtmlPropKind::Shortcut(brace, _) => brace.span.close().end(),
            HtmlPropKind::Literal(_, _, lit) => lit.span().end(),
            HtmlPropKind::Block(_, _, expr) => expr.brace_token.span.close().end(),
        }
    }
}

impl Located for HtmlBlock {
    fn start(&self) -> LineColumn {
        self.brace.span.start()
    }

    fn end(&self) -> LineColumn {
        self.brace.span.end()
    }
}

impl Located for HtmlBlockContent {
    fn start(&self) -> LineColumn {
        match self {
            Self::Expr(expr) => expr.span().start(),
            Self::Iterable(r#for, _) => r#for.span.start(),
        }
    }

    fn end(&self) -> LineColumn {
        match self {
            Self::Expr(expr) => expr,
            Self::Iterable(_, expr) => expr,
        }
        .span()
        .end()
    }

    fn loc(&self) -> Location {
        match self {
            Self::Expr(expr) => expr.loc(),
            Self::Iterable(r#for, expr) => Location { start: r#for.span.start(), end: expr.end() },
        }
    }
}

impl Located for HtmlIf {
    fn start(&self) -> LineColumn {
        self.if_token.span.start()
    }

    fn end(&self) -> LineColumn {
        self.else_branch.as_ref().map_or_else(|| self.brace.span.end(), Located::end)
    }
}

impl Located for HtmlElse {
    fn start(&self) -> LineColumn {
        match self {
            Self::If(r#else, _) => r#else,
            Self::Tree(r#else, _, _) => r#else,
        }
        .span
        .start()
    }

    fn end(&self) -> LineColumn {
        match self {
            Self::If(_, r#if) => r#if.end(),
            Self::Tree(_, brace, _) => brace.span.end(),
        }
    }

    fn loc(&self) -> Location {
        match self {
            Self::If(r#else, r#if) => Location { start: r#else.start(), end: r#if.end() },
            Self::Tree(r#else, brace, _) => {
                Location { start: r#else.span.start(), end: brace.span.end() }
            }
        }
    }
}

impl Located for HtmlFor {
    fn start(&self) -> LineColumn {
        self.for_token.span.start()
    }

    fn end(&self) -> LineColumn {
        self.brace.span.end()
    }
}

impl Located for HtmlMatch {
    fn start(&self) -> LineColumn {
        self.match_token.start()
    }

    fn end(&self) -> LineColumn {
        self.brace.span.end()
    }
}

impl Located for HtmlMatchArm {
    fn start(&self) -> LineColumn {
        self.pat.start()
    }

    fn end(&self) -> LineColumn {
        self.body.end()
    }
}

impl Located for HtmlLet {
    fn start(&self) -> LineColumn {
        self.0.let_token.start()
    }

    fn end(&self) -> LineColumn {
        self.0.semi_token.end()
    }
}

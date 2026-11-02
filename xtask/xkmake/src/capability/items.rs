// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Item index of one Rust file, built with `syn`.
//!
//! Only items written literally in the file are visible: macro expansion,
//! `cfg` evaluation and name resolution are out of scope, so a card must cite
//! where an item is declared, not where a macro generates it.
//!
//! Every item carries its path inside the file (`chain`): the names of the
//! enclosing inline modules, the implementing type or trait of an associated
//! item, and its own name. Cards name items by a path whose tail is that
//! chain (see [`ItemIndex::resolve`]).

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::{ToTokens, quote};
use sha2::{Digest, Sha256};
use syn::{Fields, ImplItem, Item, TraitItem, Type, Visibility, spanned::Spanned};

use super::source::hex;

/// One declared item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RustItem {
    /// Card symbol kind: `fn`, `struct`, `macro`, `proc_macro`, ...
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    /// Path inside the file: enclosing modules, owner type or trait, name.
    pub(crate) chain: Vec<String>,
    /// How many leading `chain` segments are inline modules.
    pub(crate) modules: usize,
    /// First line of the item including its attributes and doc comment.
    pub(crate) start_line: usize,
    /// Line of the item's name.
    pub(crate) name_line: usize,
    /// Last line of the item.
    pub(crate) end_line: usize,
    pub(crate) hashes: ItemHashes,
}

/// Token hashes of an item, insensitive to formatting, comments and doc
/// comments.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct ItemHashes {
    /// Visibility and interface: function signature, public fields,
    /// enum variants, trait item signatures, constant types.
    pub(crate) signature: String,
    /// The whole item without outer attributes (bodies included).
    pub(crate) item: String,
    /// `item` with the item's own name blanked; finds renames.
    pub(crate) shape: String,
}

impl ItemHashes {
    /// Combined hashes of the declarations one card path resolves to (for
    /// example `cfg` variants of one function), independent of their order.
    pub(crate) fn combine<'a>(parts: impl IntoIterator<Item = &'a ItemHashes>) -> Self {
        let parts: Vec<&ItemHashes> = parts.into_iter().collect();
        if let [single] = parts.as_slice() {
            return (*single).clone();
        }
        let join = |select: fn(&ItemHashes) -> &str| {
            let mut values: Vec<&str> = parts.iter().map(|part| select(part)).collect();
            values.sort_unstable();
            short_hash(&values.join(" "))
        };
        Self {
            signature: join(|h| &h.signature),
            item: join(|h| &h.item),
            shape: join(|h| &h.shape),
        }
    }
}

impl RustItem {
    /// How many trailing segments of a card path the item matches: its
    /// whole chain, or the chain without its inline modules (public paths
    /// often re-export items out of private modules); `None` otherwise.
    fn tail_score(&self, segments: &[&str]) -> Option<usize> {
        let is_tail = |chain: &[String]| {
            chain.len() <= segments.len()
                && chain
                    .iter()
                    .rev()
                    .zip(segments.iter().rev())
                    .all(|(own, wanted)| own == wanted)
        };
        if is_tail(&self.chain) {
            Some(self.chain.len())
        } else if is_tail(&self.chain[self.modules..]) {
            Some(self.chain.len() - self.modules)
        } else {
            None
        }
    }

    fn kind_matches(&self, kind: Option<&str>) -> bool {
        match kind {
            Some("macro") => matches!(self.kind, "macro" | "proc_macro"),
            Some(kind) => self.kind == kind,
            // Without a kind a proc-macro is found through its function.
            None => self.kind != "proc_macro",
        }
    }

    /// Whether the item spans lines `first..=last`.
    pub(crate) fn contains(&self, first: usize, last: usize) -> bool {
        self.start_line <= first && last <= self.end_line
    }
}

#[derive(Debug, Default)]
pub(crate) struct ItemIndex {
    pub(crate) items: Vec<RustItem>,
}

impl ItemIndex {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let file = syn::parse_file(text).map_err(|error| {
            let at = error.span().start();
            format!("{}:{}: {error}", at.line, at.column + 1)
        })?;
        let mut index = Self::default();
        index.walk(&file.items, &Scope::default());
        Ok(index)
    }

    /// Declarations a card path names: items of `kind` (any kind when
    /// `None`) whose chain, with or without its inline modules, is a tail
    /// of `path` (`a::b::C::f`), keeping only the longest matches. Several
    /// results are declarations of one path, such as `cfg` variants or
    /// methods of one type in several impl blocks; the caller treats them
    /// as one item.
    pub(crate) fn resolve(&self, path: &str, kind: Option<&str>) -> Vec<&RustItem> {
        let segments: Vec<&str> = path.split("::").collect();
        let candidates: Vec<(usize, &RustItem)> = self
            .items
            .iter()
            .filter(|item| item.kind_matches(kind))
            .filter_map(|item| Some((item.tail_score(&segments)?, item)))
            .collect();
        let best = candidates.iter().map(|(score, _)| *score).max();
        candidates
            .into_iter()
            .filter(|(score, _)| Some(*score) == best)
            .map(|(_, item)| item)
            .collect()
    }

    /// Declarations whose chain is exactly `chain`.
    pub(crate) fn exact<'a>(
        &'a self,
        chain: &'a [String],
        kind: Option<&'a str>,
    ) -> impl Iterator<Item = &'a RustItem> + 'a {
        self.items
            .iter()
            .filter(move |item| item.kind_matches(kind) && item.chain == chain)
    }

    /// Innermost item spanning lines `first..=last`, if any.
    pub(crate) fn owner_of(&self, first: usize, last: usize) -> Option<&RustItem> {
        self.items
            .iter()
            .filter(|item| item.kind != "proc_macro" && item.contains(first, last))
            .min_by_key(|item| {
                (
                    item.end_line - item.start_line,
                    std::cmp::Reverse(item.chain.len()),
                )
            })
    }

    fn push(
        &mut self,
        kind: &'static str,
        name: &syn::Ident,
        scope: &Scope,
        whole: proc_macro2::Span,
        tokens: &ItemTokens,
    ) {
        let name_line = name.span().start().line;
        let mut chain = scope.names.clone();
        chain.push(name.to_string());
        self.items.push(RustItem {
            kind,
            name: name.to_string(),
            chain,
            modules: scope.modules,
            start_line: whole.start().line.min(name_line),
            name_line,
            end_line: whole.end().line.max(name_line),
            hashes: tokens.hashes(&name.to_string()),
        });
    }

    fn walk(&mut self, items: &[Item], scope: &Scope) {
        for item in items {
            self.walk_item(item, scope);
        }
    }

    fn walk_item(&mut self, item: &Item, scope: &Scope) {
        let whole = item.span();
        match item {
            Item::Fn(f) => {
                let (vis, sig) = (&f.vis, &f.sig);
                let tokens = ItemTokens::new(item, Some(quote!(#vis #sig)));
                if let Some(macro_name) = proc_macro_name(&f.attrs, &f.sig.ident) {
                    self.push("proc_macro", &macro_name, scope, whole, &tokens);
                }
                self.push("fn", &f.sig.ident, scope, whole, &tokens);
            }
            Item::Struct(s) => {
                let signature = struct_signature(&s.vis, &s.ident, &s.generics, &s.fields);
                let tokens = ItemTokens::new(item, Some(signature));
                self.push("struct", &s.ident, scope, whole, &tokens);
            }
            Item::Union(u) => {
                let fields = Fields::Named(u.fields.clone());
                let signature = struct_signature(&u.vis, &u.ident, &u.generics, &fields);
                let tokens = ItemTokens::new(item, Some(signature));
                self.push("union", &u.ident, scope, whole, &tokens);
            }
            Item::Enum(e) => {
                let mut stripped = e.clone();
                stripped.attrs.clear();
                for variant in &mut stripped.variants {
                    variant.attrs.clear();
                    variant.discriminant = None;
                }
                let (vis, ident, generics, variants) = (
                    &stripped.vis,
                    &stripped.ident,
                    &stripped.generics,
                    &stripped.variants,
                );
                let where_clause = &generics.where_clause;
                let signature = quote!(#vis enum #ident #generics #where_clause { #variants });
                let tokens = ItemTokens::new(item, Some(signature));
                self.push("enum", &e.ident, scope, whole, &tokens);
            }
            Item::Trait(t) => {
                let (vis, unsafety, ident, generics) = (&t.vis, &t.unsafety, &t.ident, &t.generics);
                let (colon, supertraits) = (&t.colon_token, &t.supertraits);
                let where_clause = &generics.where_clause;
                let members: Vec<TokenStream> = t.items.iter().map(trait_item_signature).collect();
                let signature = quote!(
                    #vis #unsafety trait #ident #generics #colon #supertraits #where_clause
                    { #(#members)* }
                );
                let tokens = ItemTokens::new(item, Some(signature));
                self.push("trait", &t.ident, scope, whole, &tokens);
                let inner = scope.owner(t.ident.to_string());
                for member in &t.items {
                    self.walk_trait_item(member, &inner);
                }
            }
            Item::Type(t) => {
                let tokens = ItemTokens::new(item, None);
                self.push("type", &t.ident, scope, whole, &tokens);
            }
            Item::Const(c) => {
                let (vis, ident, ty) = (&c.vis, &c.ident, &c.ty);
                let tokens = ItemTokens::new(item, Some(quote!(#vis const #ident: #ty)));
                self.push("const", &c.ident, scope, whole, &tokens);
            }
            Item::Static(s) => {
                let (vis, mutability, ident, ty) = (&s.vis, &s.mutability, &s.ident, &s.ty);
                let signature = quote!(#vis static #mutability #ident: #ty);
                let tokens = ItemTokens::new(item, Some(signature));
                self.push("static", &s.ident, scope, whole, &tokens);
            }
            Item::Macro(m) => {
                if let Some(ident) = &m.ident {
                    let tokens = ItemTokens::new(item, None);
                    self.push("macro", ident, scope, whole, &tokens);
                }
            }
            Item::Mod(m) => {
                let vis = &m.vis;
                let ident = &m.ident;
                let tokens = ItemTokens::new(item, Some(quote!(#vis mod #ident)));
                self.push("mod", &m.ident, scope, whole, &tokens);
                if let Some((_, items)) = &m.content {
                    self.walk(items, &scope.module(m.ident.to_string()));
                }
            }
            Item::Impl(imp) => {
                let inner = match type_name(&imp.self_ty) {
                    Some(name) => scope.owner(name),
                    None => scope.clone(),
                };
                for member in &imp.items {
                    self.walk_impl_item(member, &inner);
                }
            }
            Item::ForeignMod(foreign) => {
                for member in &foreign.items {
                    if let syn::ForeignItem::Fn(f) = member {
                        let (vis, sig) = (&f.vis, &f.sig);
                        let tokens = ItemTokens::new(member, Some(quote!(#vis #sig)));
                        self.push("fn", &f.sig.ident, scope, member.span(), &tokens);
                    }
                }
            }
            Item::Verbatim(tokens) => self.push_verbatim(tokens, scope),
            _ => {}
        }
    }

    /// Items syn does not model, e.g. linkme's `pub static NAME: [T];`:
    /// recognised by keyword and name only, hashed as written.
    fn push_verbatim(&mut self, tokens: &TokenStream, scope: &Scope) {
        const KEYWORDS: &[&str] = &[
            "fn", "struct", "enum", "union", "trait", "type", "const", "static", "mod",
        ];
        let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
        let (Some(first), Some(last)) = (trees.first(), trees.last()) else {
            return;
        };
        let Some((index, kind)) = trees
            .iter()
            .enumerate()
            .find_map(|(index, tree)| match tree {
                TokenTree::Ident(ident) => KEYWORDS
                    .iter()
                    .find(|word| ident == *word)
                    .map(|word| (index, *word)),
                _ => None,
            })
        else {
            return;
        };
        let name = trees[index + 1..].iter().find_map(|tree| match tree {
            TokenTree::Ident(ident) if ident != "mut" && ident != "unsafe" => Some(ident.clone()),
            _ => None,
        });
        let Some(name) = name else {
            return;
        };
        let text = normalized(tokens.clone());
        let item_tokens = ItemTokens {
            signature: text.clone(),
            item: text,
        };
        let whole = first
            .span()
            .join(last.span())
            .unwrap_or_else(|| first.span());
        self.push(kind, &name, scope, whole, &item_tokens);
    }

    fn walk_impl_item(&mut self, member: &ImplItem, scope: &Scope) {
        let whole = member.span();
        match member {
            ImplItem::Fn(f) => {
                let (vis, defaultness, sig) = (&f.vis, &f.defaultness, &f.sig);
                let tokens = ItemTokens::new(member, Some(quote!(#vis #defaultness #sig)));
                self.push("fn", &f.sig.ident, scope, whole, &tokens);
            }
            ImplItem::Const(c) => {
                let (vis, ident, ty) = (&c.vis, &c.ident, &c.ty);
                let tokens = ItemTokens::new(member, Some(quote!(#vis const #ident: #ty)));
                self.push("const", &c.ident, scope, whole, &tokens);
            }
            ImplItem::Type(t) => {
                let tokens = ItemTokens::new(member, None);
                self.push("type", &t.ident, scope, whole, &tokens);
            }
            _ => {}
        }
    }

    fn walk_trait_item(&mut self, member: &TraitItem, scope: &Scope) {
        let whole = member.span();
        let tokens = ItemTokens::new(member, Some(trait_item_signature(member)));
        match member {
            TraitItem::Fn(f) => self.push("fn", &f.sig.ident, scope, whole, &tokens),
            TraitItem::Const(c) => self.push("const", &c.ident, scope, whole, &tokens),
            TraitItem::Type(t) => self.push("type", &t.ident, scope, whole, &tokens),
            _ => {}
        }
    }
}

/// Names enclosing an item: inline modules first, then an owner.
#[derive(Clone, Default)]
struct Scope {
    names: Vec<String>,
    modules: usize,
}

impl Scope {
    fn module(&self, name: String) -> Self {
        let mut names = self.names.clone();
        names.push(name);
        Self {
            modules: names.len(),
            names,
        }
    }

    fn owner(&self, name: String) -> Self {
        let mut names = self.names.clone();
        names.push(name);
        Self {
            names,
            modules: self.modules,
        }
    }
}

/// Token text of an item, before hashing.
struct ItemTokens {
    signature: String,
    item: String,
}

impl ItemTokens {
    /// `signature: None` uses the whole item: aliases and macros are all interface.
    fn new(item: &impl ToTokens, signature: Option<TokenStream>) -> Self {
        // Outer attributes (docs, cfg, derives) print first; drop them so the
        // hash follows the code.
        let item = normalized(skip_outer_attributes(item.to_token_stream()));
        Self {
            signature: signature.map_or_else(|| item.clone(), normalized),
            item,
        }
    }

    fn hashes(&self, name: &str) -> ItemHashes {
        ItemHashes {
            signature: short_hash(&self.signature),
            item: short_hash(&self.item),
            shape: short_hash(&blank_identifier(&self.item, name)),
        }
    }
}

/// Drops leading `#[...]` attributes.
fn skip_outer_attributes(tokens: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    let mut start = 0;
    while let (Some(TokenTree::Punct(punct)), Some(TokenTree::Group(group))) =
        (trees.get(start), trees.get(start + 1))
    {
        if punct.as_char() != '#' || group.delimiter() != Delimiter::Bracket {
            break;
        }
        start += 2;
    }
    trees.into_iter().skip(start).collect()
}

/// Token text with every doc attribute (`#[doc = ...]`, `#![doc = ...]`)
/// removed at any depth, so doc-comment edits do not change a hash; the
/// quotes that cite doc comments are checked separately.
fn normalized(tokens: TokenStream) -> String {
    strip_docs(tokens).to_string()
}

fn strip_docs(tokens: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    let mut out = Vec::with_capacity(trees.len());
    let mut index = 0;
    while index < trees.len() {
        if let TokenTree::Punct(punct) = &trees[index]
            && punct.as_char() == '#'
        {
            let bang =
                matches!(&trees.get(index + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!');
            let group_index = index + 1 + usize::from(bang);
            if let Some(TokenTree::Group(group)) = trees.get(group_index)
                && group.delimiter() == Delimiter::Bracket
                && matches!(group.stream().into_iter().next(), Some(TokenTree::Ident(ident)) if ident == "doc")
            {
                index = group_index + 1;
                continue;
            }
        }
        match &trees[index] {
            TokenTree::Group(group) => {
                let mut rebuilt =
                    proc_macro2::Group::new(group.delimiter(), strip_docs(group.stream()));
                rebuilt.set_span(group.span());
                out.push(TokenTree::Group(rebuilt));
            }
            other => out.push(other.clone()),
        }
        index += 1;
    }
    out.into_iter().collect()
}

pub(crate) fn short_hash(text: &str) -> String {
    hex(&Sha256::digest(text.trim().as_bytes())[..8])
}

/// Replaces whole-word occurrences of `name` in token text.
fn blank_identifier(text: &str, name: &str) -> String {
    text.split(' ')
        .map(|token| if token == name { "_" } else { token })
        .collect::<Vec<_>>()
        .join(" ")
}

fn struct_signature(
    vis: &Visibility,
    ident: &syn::Ident,
    generics: &syn::Generics,
    fields: &Fields,
) -> TokenStream {
    let visible: Vec<TokenStream> = fields
        .iter()
        .enumerate()
        .filter(|(_, field)| !matches!(field.vis, Visibility::Inherited))
        .map(|(index, field)| {
            let (vis, ty) = (&field.vis, &field.ty);
            match &field.ident {
                Some(name) => quote!(#vis #name: #ty,),
                None => {
                    let position = syn::Index::from(index);
                    quote!(#vis #position: #ty,)
                }
            }
        })
        .collect();
    let where_clause = &generics.where_clause;
    quote!(#vis struct #ident #generics #where_clause { #(#visible)* })
}

fn trait_item_signature(member: &TraitItem) -> TokenStream {
    match member {
        TraitItem::Fn(f) => {
            let sig = &f.sig;
            quote!(#sig;)
        }
        TraitItem::Const(c) => {
            let (ident, generics, ty) = (&c.ident, &c.generics, &c.ty);
            quote!(const #ident #generics: #ty;)
        }
        TraitItem::Type(t) => {
            let (ident, generics, colon, bounds) =
                (&t.ident, &t.generics, &t.colon_token, &t.bounds);
            quote!(type #ident #generics #colon #bounds;)
        }
        other => other.to_token_stream(),
    }
}

fn type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(reference) => type_name(&reference.elem),
        Type::Paren(paren) => type_name(&paren.elem),
        Type::Group(group) => type_name(&group.elem),
        _ => None,
    }
}

/// Name under which a proc-macro function is invoked.
fn proc_macro_name(attrs: &[syn::Attribute], function: &syn::Ident) -> Option<syn::Ident> {
    attrs.iter().find_map(|attr| {
        let path = attr.path();
        if path.is_ident("proc_macro") || path.is_ident("proc_macro_attribute") {
            Some(function.clone())
        } else if path.is_ident("proc_macro_derive") {
            attr.parse_args_with(|input: syn::parse::ParseStream<'_>| {
                let name: syn::Ident = input.parse()?;
                let _: TokenStream = input.parse()?;
                Ok(name)
            })
            .ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{ItemHashes, ItemIndex};

    const SOURCE: &str = r#"//! Crate docs.

/// A ring.
#[derive(Debug)]
pub struct Ring<T> {
    pub capacity: usize,
    items: Vec<T>,
}

impl<T> Ring<T> {
    /// Creates a ring.
    pub fn new(
        capacity: usize,
    ) -> Self {
        Self { capacity, items: Vec::new() }
    }
}

pub trait Queue {
    fn push(&mut self, value: u32);
}

#[macro_export]
macro_rules! ring {
    () => {};
}

#[proc_macro_derive(Ringish, attributes(ring))]
pub fn derive_ringish(input: TokenStream) -> TokenStream { input }

mod inner {
    pub const LIMIT: usize = 4;
    pub fn new() {}
}

/// Registered handlers.
#[distributed_slice]
pub static HANDLERS: [fn(usize) -> bool];
"#;

    fn hashes(text: &str, path: &str, kind: &str) -> ItemHashes {
        let index = ItemIndex::parse(text).unwrap();
        let found = index.resolve(path, Some(kind));
        assert_eq!(found.len(), 1, "{path}");
        found[0].hashes.clone()
    }

    #[test]
    fn items_span_attributes_docs_and_bodies() {
        let index = ItemIndex::parse(SOURCE).unwrap();
        let ring = index.resolve("k::Ring", Some("struct"))[0];
        assert_eq!((ring.start_line, ring.name_line, ring.end_line), (3, 5, 8));
        let new = index.resolve("k::Ring::new", Some("fn"));
        assert_eq!(new.len(), 1);
        assert_eq!((new[0].start_line, new[0].end_line), (11, 16));
        assert_eq!(index.owner_of(15, 15).unwrap().chain, ["Ring", "new"]);
        assert!(index.owner_of(1, 1).is_none());
        // A path whose owner matches nothing falls back to a free item of
        // that name; the stamp then pins the exact chain.
        assert_eq!(
            index.resolve("k::Other::new", Some("fn"))[0].chain,
            ["inner", "new"]
        );
        assert!(index.resolve("k::Other::missing", None).is_empty());
        // Inline modules may be left out of a re-exported path.
        assert_eq!(index.resolve("k::LIMIT", None)[0].chain, ["inner", "LIMIT"]);
        assert_eq!(
            index.resolve("k::Queue::push", None)[0].chain,
            ["Queue", "push"]
        );
        assert!(!index.resolve("k::ring", Some("macro")).is_empty());
        assert!(!index.resolve("k::Ringish", Some("macro")).is_empty());
        assert_eq!(index.resolve("k::derive_ringish", None).len(), 1);
        assert_eq!(
            index.resolve("k::inner::LIMIT", None)[0].chain,
            ["inner", "LIMIT"]
        );
        assert_eq!(
            index.resolve("k::inner::new", None)[0].chain,
            ["inner", "new"]
        );
        let handlers = index.resolve("k::HANDLERS", Some("static"))[0];
        assert_eq!((handlers.start_line, handlers.end_line), (36, 38));
    }

    #[test]
    fn hashes_ignore_comments_and_docs_and_track_signatures_and_bodies() {
        let original = hashes(SOURCE, "k::Ring::new", "fn");

        let commented = SOURCE.replace(
            "/// Creates a ring.",
            "/// Creates a ring buffer.\n    // note",
        );
        assert_eq!(hashes(&commented, "k::Ring::new", "fn"), original);

        let body = SOURCE.replace(
            "items: Vec::new() }",
            "items: Vec::with_capacity(capacity) }",
        );
        let changed = hashes(&body, "k::Ring::new", "fn");
        assert_eq!(changed.signature, original.signature);
        assert_ne!(changed.item, original.item);

        let signature = SOURCE.replace(
            "capacity: usize,\n    ) -> Self",
            "capacity: u32,\n    ) -> Self",
        );
        assert_ne!(
            hashes(&signature, "k::Ring::new", "fn").signature,
            original.signature
        );

        let ring = hashes(SOURCE, "k::Ring", "struct");
        let field_doc = SOURCE.replace("    items: Vec<T>,", "    /// Items.\n    items: Vec<T>,");
        assert_eq!(hashes(&field_doc, "k::Ring", "struct"), ring);
        let private_field = SOURCE.replace("items: Vec<T>,", "items: Vec<T>,\n    len: usize,");
        let changed = hashes(&private_field, "k::Ring", "struct");
        assert_eq!(changed.signature, ring.signature);
        assert_ne!(changed.item, ring.item);

        let queue = hashes(SOURCE, "k::Queue", "trait");
        let renamed = SOURCE.replace("pub trait Queue", "pub trait Fifo");
        assert_eq!(hashes(&renamed, "k::Fifo", "trait").shape, queue.shape);
    }

    #[test]
    fn combined_hashes_do_not_depend_on_order() {
        let a = hashes(SOURCE, "k::Ring::new", "fn");
        let b = hashes(SOURCE, "k::Ring", "struct");
        assert_eq!(ItemHashes::combine([&a, &b]), ItemHashes::combine([&b, &a]));
        assert_eq!(ItemHashes::combine([&a]), a);
    }

    #[test]
    fn parse_errors_carry_a_position() {
        let error = ItemIndex::parse("fn broken( {").unwrap_err();
        assert!(error.starts_with("1:"), "{error}");
    }
}

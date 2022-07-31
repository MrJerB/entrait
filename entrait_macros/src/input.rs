//!
//! inputs to procedural macros
//!
//!
//!

use crate::token_util::push_tokens;

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::parse::{Parse, ParseStream};

pub enum Input {
    Fn(InputFn),
    Trait(syn::ItemTrait),
    Mod(InputMod),
}

pub struct InputFn {
    pub fn_attrs: Vec<syn::Attribute>,
    pub fn_vis: syn::Visibility,
    pub fn_sig: syn::Signature,
    // don't try to parse fn_body, just pass through the tokens:
    pub fn_body: proc_macro2::TokenStream,
}

pub struct InputMod {
    pub attrs: Vec<syn::Attribute>,
    pub vis: syn::Visibility,
    pub mod_token: syn::token::Mod,
    pub ident: syn::Ident,
    pub brace_token: syn::token::Brace,
    pub items: Vec<ModItem>,
}

impl ToTokens for InputMod {
    fn to_tokens(&self, stream: &mut TokenStream) {
        for attr in &self.attrs {
            push_tokens!(stream, attr);
        }
        push_tokens!(stream, self.vis, self.mod_token, self.ident);
        self.brace_token.surround(stream, |stream| {
            for item in &self.items {
                item.to_tokens(stream);
            }
        });
    }
}

pub enum ModItem {
    Fn(InputFn),
    Unknown(ItemUnknown),
}

impl ModItem {
    // We include all functions that have a visibility keyword into the trait
    pub fn filter_pub_fn(&self) -> Option<&InputFn> {
        match self {
            Self::Fn(input_fn) => match input_fn.fn_vis {
                syn::Visibility::Public(_)
                | syn::Visibility::Crate(_)
                | syn::Visibility::Restricted(_) => Some(input_fn),
                syn::Visibility::Inherited => None,
            },
            _ => None,
        }
    }
}

impl ToTokens for ModItem {
    fn to_tokens(&self, stream: &mut TokenStream) {
        match self {
            ModItem::Fn(InputFn {
                fn_attrs,
                fn_vis,
                fn_sig,
                fn_body,
            }) => {
                for attr in fn_attrs {
                    push_tokens!(stream, attr);
                }
                push_tokens!(stream, fn_vis, fn_sig, fn_body);
            }
            ModItem::Unknown(unknown) => {
                unknown.to_tokens(stream);
            }
        }
    }
}

pub struct ItemUnknown {
    attrs: Vec<syn::Attribute>,
    vis: syn::Visibility,
    tokens: TokenStream,
}

impl ToTokens for ItemUnknown {
    fn to_tokens(&self, stream: &mut TokenStream) {
        for attr in &self.attrs {
            attr.to_tokens(stream);
        }
        push_tokens!(stream, self.vis, self.tokens);
    }
}

impl Parse for Input {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(syn::Attribute::parse_outer)?;
        let vis = input.parse()?;

        // BUG (In theory): missing "unsafe" and "auto" traits
        if input.peek(syn::token::Trait) {
            let item_trait: syn::ItemTrait = input.parse()?;

            Ok(Input::Trait(syn::ItemTrait {
                attrs,
                vis,
                ..item_trait
            }))
        } else if input.peek(syn::token::Mod) {
            let mod_token = input.parse()?;
            let ident = input.parse()?;

            let lookahead = input.lookahead1();
            if lookahead.peek(syn::token::Brace) {
                let content;
                let brace_token = syn::braced!(content in input);

                let mut items = vec![];

                while !content.is_empty() {
                    items.push(content.parse()?);
                }

                Ok(Input::Mod(InputMod {
                    attrs,
                    vis,
                    mod_token,
                    ident,
                    brace_token,
                    items,
                }))
            } else {
                Err(lookahead.error())
            }
        } else {
            let fn_sig: syn::Signature = input.parse()?;
            let fn_body = input.parse()?;

            Ok(Input::Fn(InputFn {
                fn_attrs: attrs,
                fn_vis: vis,
                fn_sig,
                fn_body,
            }))
        }
    }
}

impl Parse for ModItem {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(syn::Attribute::parse_outer)?;
        let vis: syn::Visibility = input.parse()?;
        let unknown = input.fork();
        let ahead = input.fork();

        if input.peek(syn::token::Fn) || peek_signature(&ahead) {
            let sig: syn::Signature = input.parse()?;
            if input.peek(syn::token::Semi) {
                let _ = input.parse::<syn::token::Semi>()?;
                Ok(ModItem::Unknown(ItemUnknown {
                    attrs,
                    vis,
                    tokens: verbatim_between(unknown, input),
                }))
            } else {
                let fn_body = parse_matched_braces_or_ending_semi(input)?;
                Ok(ModItem::Fn(InputFn {
                    fn_attrs: attrs,
                    fn_vis: vis,
                    fn_sig: sig,
                    fn_body,
                }))
            }
        } else {
            let tokens = parse_matched_braces_or_ending_semi(input)?;
            Ok(ModItem::Unknown(ItemUnknown { attrs, vis, tokens }))
        }
    }
}

fn peek_signature(input: ParseStream) -> bool {
    let fork = input.fork();
    fork.parse::<Option<syn::token::Const>>().is_ok()
        && fork.parse::<Option<syn::token::Async>>().is_ok()
        && fork.parse::<Option<syn::token::Unsafe>>().is_ok()
        && fork.parse::<Option<syn::Abi>>().is_ok()
        && fork.peek(syn::token::Fn)
}

fn verbatim_between<'a>(begin: syn::parse::ParseBuffer<'a>, end: ParseStream<'a>) -> TokenStream {
    let end = end.cursor();
    let mut cursor = begin.cursor();
    let mut tokens = TokenStream::new();
    while cursor != end {
        let (tt, next) = cursor.token_tree().unwrap();
        tokens.extend(std::iter::once(tt));
        cursor = next;
    }
    tokens
}

fn parse_matched_braces_or_ending_semi(input: ParseStream) -> syn::Result<TokenStream> {
    let mut tokens = input.step(|cursor| {
        let mut tokens = TokenStream::new();

        use proc_macro2::Delimiter;
        use proc_macro2::TokenTree;

        let mut rest = *cursor;

        while let Some((tt, next)) = rest.token_tree() {
            match &tt {
                TokenTree::Group(group) => {
                    let is_brace = group.delimiter() == Delimiter::Brace;
                    tokens.extend(std::iter::once(tt));
                    if is_brace {
                        return Ok((tokens, next));
                    }
                }
                TokenTree::Punct(punct) => {
                    let is_semi = punct.as_char() == ';';
                    tokens.extend(std::iter::once(tt));
                    if is_semi {
                        return Ok((tokens, next));
                    }
                }
                _ => {
                    tokens.extend(std::iter::once(tt));
                }
            }
            rest = next;
        }
        Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "Read past the end",
        ))
    })?;

    while input.peek(syn::token::Semi) {
        let semi = input.parse::<syn::token::Semi>()?;
        semi.to_tokens(&mut tokens);
    }

    Ok(tokens)
}

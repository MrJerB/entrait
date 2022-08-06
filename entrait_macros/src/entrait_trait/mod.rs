//! Implementation for invoking entrait on a trait!

pub mod input_attr;
mod out_trait;

use input_attr::EntraitTraitAttr;

use crate::analyze_generics::TraitFn;
use crate::generics;
use crate::generics::TraitDependencyMode;
use crate::idents::GenericIdents;
use crate::impl_fn_codegen::opt_async_trait_attribute;
use crate::input::FnInputMode;
use crate::input::LiteralAttrs;
use crate::opt::*;
use crate::token_util::*;
use crate::trait_codegen::Supertraits;
use crate::trait_codegen::TraitCodegen;

use proc_macro2::TokenStream;
use quote::ToTokens;
use quote::{quote, quote_spanned};

use self::out_trait::OutTrait;

pub fn output_tokens(
    attr: EntraitTraitAttr,
    item_trait: syn::ItemTrait,
) -> syn::Result<TokenStream> {
    let trait_ident_span = item_trait.ident.span();
    let contains_async = item_trait.items.iter().any(|item| match item {
        syn::TraitItem::Method(method) => method.sig.asyncness.is_some(),
        _ => false,
    });
    let impl_attrs = item_trait
        .attrs
        .iter()
        .filter(|attr| {
            matches!(
                attr.path.segments.last(),
                Some(last_segment) if last_segment.ident == "async_trait"
            )
        })
        .cloned()
        .collect::<Vec<_>>();

    let out_trait = out_trait::analyze_trait(item_trait, &attr.crate_idents, &attr.opts)?;
    let trait_dependency_mode = TraitDependencyMode::Generic(GenericIdents::new(
        &attr.crate_idents,
        out_trait.ident.span(),
    ));
    let generic_idents = match &trait_dependency_mode {
        TraitDependencyMode::Generic(idents) => idents,
        _ => panic!(),
    };

    let mut impl_async_trait_attr =
        opt_async_trait_attribute(&attr.opts, &attr.crate_idents, out_trait.fns.iter());
    if !impl_attrs.is_empty() {
        impl_async_trait_attr = None;
    }

    let delegation_trait_def =
        gen_delegation_trait_def(&out_trait, &trait_dependency_mode, &generic_idents, &attr)?;

    let trait_def = TraitCodegen {
        crate_idents: &attr.crate_idents,
        opts: &attr.opts,
    }
    .gen_trait_def(
        &out_trait.vis,
        &out_trait.ident,
        &out_trait.generics,
        &out_trait.supertraits,
        &trait_dependency_mode,
        &out_trait.fns,
        &FnInputMode::RawTrait(LiteralAttrs(&out_trait.attrs)),
    )?;

    let trait_ident = &out_trait.ident;
    let params = out_trait.generics.impl_params_from_idents(
        &generic_idents,
        &generics::ImplIndirection::None,
        generics::UseAssociatedFuture(false),
    );
    let args = out_trait
        .generics
        .arguments(&generics::ImplIndirection::None);
    let self_ty = generic_idents.impl_path(trait_ident_span);
    let where_clause = ImplWhereClause {
        out_trait: &out_trait,
        contains_async,
        trait_generics: &out_trait.generics,
        generic_idents: &generic_idents,
        attr: &attr,
        span: trait_ident_span,
    };

    let impl_assoc_types = out_trait
        .fns
        .iter()
        .filter_map(|trait_fn| trait_fn.entrait_sig.associated_fut_impl.as_ref());

    let method_items = out_trait
        .fns
        .iter()
        .map(|trait_fn| gen_delegation_method(trait_fn, generic_idents, &attr));

    Ok(quote! {
        #trait_def

        #delegation_trait_def

        #(#impl_attrs)*
        #impl_async_trait_attr
        impl #params #trait_ident #args for #self_ty #where_clause {
            #(#impl_assoc_types)*
            #(#method_items)*
        }
    })
}

fn gen_delegation_trait_def(
    out_trait: &OutTrait,
    trait_dependency_mode: &TraitDependencyMode,
    generic_idents: &GenericIdents,
    attr: &EntraitTraitAttr,
) -> syn::Result<Option<TokenStream>> {
    let entrait = &generic_idents.crate_idents.entrait;

    match &attr.delegation_kind {
        Some(SpanOpt(DelegationKind::ByTraitStatic(trait_ident), _)) => {
            let span = trait_ident.span();
            let impl_t = &generic_idents.impl_t;
            Ok(Some(quote_spanned! { span=>
                pub trait #trait_ident<#impl_t> {
                    type By: for<'a> ::#entrait::BorrowImpl<'a, #impl_t>;
                }
            }))
        }
        Some(SpanOpt(DelegationKind::ByTraitDyn(trait_ident), _)) => {
            let mut trait_copy = out_trait.clone();

            trait_copy.ident = trait_ident.clone();
            trait_copy.generics.params.insert(
                0,
                syn::parse_quote! {
                    EntraitT
                },
            );
            for trait_fn in trait_copy.fns.iter_mut() {
                if !matches!(trait_fn.sig().inputs.first(), Some(syn::FnArg::Receiver(_))) {
                    continue;
                }

                trait_fn.entrait_sig.sig.inputs.insert(
                    1,
                    syn::parse_quote! {
                        __impl: &::#entrait::Impl<EntraitT>
                    },
                );
            }

            let no_mock_opts = Opts {
                unimock: None,
                mockall: None,
                ..attr.opts
            };

            let trait_def = TraitCodegen {
                crate_idents: &attr.crate_idents,
                opts: &no_mock_opts,
            }
            .gen_trait_def(
                &trait_copy.vis,
                &trait_copy.ident,
                &trait_copy.generics,
                &Supertraits::Some {
                    colon_token: syn::token::Colon::default(),
                    bounds: syn::parse_quote! { 'static },
                },
                trait_dependency_mode,
                &trait_copy.fns,
                &FnInputMode::RawTrait(LiteralAttrs(&[])),
            )?;

            Ok(Some(trait_def))
        }
        _ => Ok(None),
    }
}

fn gen_delegation_method(
    trait_fn: &TraitFn,
    generic_idents: &GenericIdents,
    attr: &EntraitTraitAttr,
) -> TokenStream {
    let fn_sig = &trait_fn.sig();
    let fn_ident = &fn_sig.ident;
    let impl_t = &generic_idents.impl_t;
    let entrait = &attr.crate_idents.entrait;

    let arguments = fn_sig.inputs.iter().filter_map(|arg| match arg {
        syn::FnArg::Receiver(_) => None,
        syn::FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
            syn::Pat::Ident(pat_ident) => Some(pat_ident.ident.to_token_stream()),
            _ => panic!("Found a non-ident pattern, this should be handled in signature.rs"),
        },
    });
    let opt_dot_await = trait_fn.originally_async.then(|| quote! { .await });
    let core = &generic_idents.crate_idents.core;

    match &attr.delegation_kind {
        Some(SpanOpt(DelegationKind::ByTraitStatic(_), _)) => {
            quote! {
                #fn_sig {
                    <#impl_t::By as ::#entrait::BorrowImpl<#impl_t>>::Target::from(self).#fn_ident(#(#arguments),*) #opt_dot_await
                }
            }
        }
        Some(SpanOpt(DelegationKind::ByTraitDyn(trait_ident), _)) => {
            quote! {
                #fn_sig {
                    <#impl_t as ::#core::borrow::Borrow<dyn #trait_ident<#impl_t>>>::borrow(&*self)
                        .#fn_ident(self, #(#arguments),*) #opt_dot_await
                }
            }
        }
        Some(SpanOpt(DelegationKind::ByBorrow, _)) => {
            quote! {
                #fn_sig {
                    self.as_ref().borrow().#fn_ident(#(#arguments),*) #opt_dot_await
                }
            }
        }
        _ => {
            quote! {
                #fn_sig {
                    self.as_ref().#fn_ident(#(#arguments),*) #opt_dot_await
                }
            }
        }
    }
}

struct ImplWhereClause<'g, 'c> {
    out_trait: &'g OutTrait,
    contains_async: bool,
    trait_generics: &'g generics::TraitGenerics,
    generic_idents: &'g GenericIdents<'c>,
    attr: &'g EntraitTraitAttr,
    span: proc_macro2::Span,
}

impl<'g, 'c> ImplWhereClause<'g, 'c> {
    fn push_impl_t_bounds(&self, stream: &mut TokenStream) {
        use syn::token::*;

        push_tokens!(stream, self.generic_idents.impl_t, Colon(self.span));

        match &self.attr.delegation_kind {
            Some(SpanOpt(DelegationKind::ByTraitStatic(trait_ident), _)) => {
                push_tokens!(
                    stream,
                    trait_ident,
                    Lt(self.span),
                    self.generic_idents.impl_t,
                    Gt(self.span)
                );
            }
            Some(SpanOpt(DelegationKind::ByTraitDyn(trait_ident), _)) => {
                self.push_core_borrow_borrow(stream);
                push_tokens!(
                    stream,
                    // Generic arguments:
                    Lt(self.span),
                    Dyn(self.span),
                    trait_ident,
                    Lt(self.span),
                    self.generic_idents.impl_t,
                    Gt(self.span),
                    Gt(self.span)
                );

                if self.contains_async {
                    push_tokens!(stream, self.plus_send(), self.plus_sync());
                }
                push_tokens!(stream, self.plus_static());
            }
            Some(SpanOpt(DelegationKind::ByBorrow, _)) => {
                self.push_core_borrow_borrow(stream);
                push_tokens!(
                    stream,
                    Lt(self.span),
                    Dyn(self.span),
                    self.trait_with_arguments(),
                    Gt(self.span)
                );

                if self.contains_async {
                    push_tokens!(stream, self.plus_send(), self.plus_sync());
                }
                push_tokens!(stream, self.plus_static());
            }
            _ => {
                push_tokens!(stream, self.trait_with_arguments(), self.plus_sync());
            }
        }
    }

    fn push_delegate_borrow_impl_ref_bounds(&self, stream: &mut TokenStream) {
        use syn::token::*;
        use syn::Ident;

        let span = self.span;

        let lifetime = syn::Lifetime::new("'entrait_a", self.span);

        push_tokens!(
            stream,
            // for<'a>
            For(span),
            Lt(span),
            lifetime,
            Gt(span),
            // <T::By as ::entrait::BorrowImpl<'a, T>>
            Lt(span),
            self.generic_idents.impl_t,
            Colon2(span),
            Ident::new("By", span),
            As(span),
            Colon2(span),
            self.attr.crate_idents.entrait,
            Colon2(span),
            Ident::new("BorrowImpl", span),
            Lt(span),
            lifetime,
            Comma(span),
            self.generic_idents.impl_t,
            Gt(span),
            Gt(span),
            // ::Target: #trait_ident
            Colon2(span),
            Ident::new("Target", span),
            Colon(span),
            self.out_trait.ident,
            self.plus_send(),
            self.plus_sync()
        );
    }

    fn push_core_borrow_borrow(&self, stream: &mut TokenStream) {
        use syn::token::*;
        push_tokens!(
            stream,
            Colon2(self.span),
            self.generic_idents.crate_idents.core,
            Colon2(self.span),
            syn::Ident::new("borrow", self.span),
            Colon2(self.span),
            syn::Ident::new("Borrow", self.span)
        );
    }

    fn trait_with_arguments(&self) -> TokenPair<impl ToTokens + '_, impl ToTokens + '_> {
        TokenPair(
            &self.out_trait.ident,
            self.trait_generics
                .arguments(&generics::ImplIndirection::None),
        )
    }

    fn plus_static(&self) -> TokenPair<impl ToTokens, impl ToTokens> {
        TokenPair(
            syn::token::Add(self.span),
            syn::Lifetime::new("'static", self.span),
        )
    }

    fn plus_send(&self) -> TokenPair<impl ToTokens, impl ToTokens> {
        TokenPair(
            syn::token::Add(self.span),
            syn::Ident::new("Send", self.span),
        )
    }

    fn plus_sync(&self) -> TokenPair<impl ToTokens, impl ToTokens> {
        TokenPair(
            syn::token::Add(self.span),
            syn::Ident::new("Sync", self.span),
        )
    }
}

impl<'g, 'c> quote::ToTokens for ImplWhereClause<'g, 'c> {
    fn to_tokens(&self, stream: &mut TokenStream) {
        let mut punctuator = Punctuator::new(
            stream,
            syn::token::Where(self.span),
            syn::token::Comma(self.span),
            EmptyToken,
        );

        // Bounds on the `T` in `Impl<T>`:
        punctuator.push_fn(|stream| {
            self.push_impl_t_bounds(stream);
        });

        // Trait delegation bounds:
        if let Some(SpanOpt(DelegationKind::ByTraitStatic(_), _)) = &self.attr.delegation_kind {
            punctuator.push_fn(|stream| {
                self.push_delegate_borrow_impl_ref_bounds(stream);
            });
        }

        for predicate in &self.trait_generics.where_predicates {
            punctuator.push(predicate);
        }
    }
}

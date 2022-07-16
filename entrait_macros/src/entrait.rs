//! # entrait_macros
//!
//! Procedural macros used by entrait.

use proc_macro2::Span;
use proc_macro2::TokenStream;
use quote::quote;
use quote::quote_spanned;
use syn::spanned::Spanned;

use crate::generics;
use crate::input::*;
use crate::signature;
use crate::signature::EntraitSignature;

pub fn invoke(
    attr: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
    attr_modifier: impl FnOnce(&mut EntraitAttr),
) -> proc_macro::TokenStream {
    let mut attr = syn::parse_macro_input!(attr as EntraitAttr);
    let input_fn = syn::parse_macro_input!(input as InputFn);

    attr_modifier(&mut attr);

    let output = match output_tokens(&attr, input_fn) {
        Ok(token_stream) => token_stream,
        Err(err) => err.into_compile_error(),
    };

    if attr.debug_value() {
        println!("{}", output);
    }

    proc_macro::TokenStream::from(output)
}

fn output_tokens(attr: &EntraitAttr, input_fn: InputFn) -> syn::Result<proc_macro2::TokenStream> {
    let generics = generics::analyze_generics(&input_fn, attr)?;
    let entrait_sig = signature::SignatureConverter::new(attr, &input_fn, &generics.deps).convert();
    let trait_def = gen_trait_def(attr, &input_fn, &entrait_sig, &generics)?;
    let impl_blocks = gen_impl_blocks(attr, &input_fn, &entrait_sig, &generics)?;

    let InputFn {
        fn_attrs,
        fn_vis,
        fn_sig,
        fn_body,
        ..
    } = input_fn;

    Ok(quote! {
        #(#fn_attrs)* #fn_vis #fn_sig #fn_body
        #trait_def
        #impl_blocks
    })
}

fn gen_trait_def(
    attr: &EntraitAttr,
    input_fn: &InputFn,
    entrait_sig: &EntraitSignature,
    generics: &generics::Generics,
) -> syn::Result<proc_macro2::TokenStream> {
    let span = attr.trait_ident.span();
    let trait_def = gen_trait_def_no_mock(attr, input_fn, entrait_sig, generics)?;

    Ok(
        match (
            attr.opt_unimock_attribute(input_fn, &generics.deps),
            attr.opt_mockall_automock_attribute(),
        ) {
            (None, None) => trait_def,
            (unimock, automock) => quote_spanned! { span=>
                #unimock
                #automock
                #trait_def
            },
        },
    )
}

fn gen_trait_def_no_mock(
    attr: &EntraitAttr,
    input_fn: &InputFn,
    entrait_sig: &EntraitSignature,
    generics: &generics::Generics,
) -> syn::Result<proc_macro2::TokenStream> {
    let trait_visibility = &attr.trait_visibility;
    let trait_ident = &attr.trait_ident;
    let span = trait_ident.span();
    let trait_fn_sig = &entrait_sig.sig;
    let where_clause = &generics.trait_generics.where_clause;
    let generics = &generics.trait_generics;

    Ok(
        if let Some(associated_fut) = &entrait_sig.associated_fut_decl {
            quote_spanned! { span=>
                #trait_visibility trait #trait_ident #generics #where_clause {
                    #associated_fut
                    #trait_fn_sig;
                }
            }
        } else {
            let opt_async_trait_attr = input_fn.opt_async_trait_attribute(attr);

            quote_spanned! { span=>
                #opt_async_trait_attr
                #trait_visibility trait #trait_ident #generics #where_clause {
                    #trait_fn_sig;
                }
            }
        },
    )
}

///
/// Generate code like
///
/// ```no_compile
/// impl<__T: ::entrait::Impl + Deps> Trait for __T {
///     fn the_func(&self, args...) {
///         the_func(self, args)
///     }
/// }
/// ```
///
fn gen_impl_blocks(
    attr: &EntraitAttr,
    input_fn: &InputFn,
    entrait_sig: &EntraitSignature,
    generics: &generics::Generics,
) -> syn::Result<proc_macro2::TokenStream> {
    let EntraitAttr { trait_ident, .. } = attr;
    let InputFn { fn_sig, .. } = input_fn;

    let span = trait_ident.span();

    let mut input_fn_ident = fn_sig.ident.clone();
    input_fn_ident.set_span(span);

    let async_trait_attribute = input_fn.opt_async_trait_attribute(attr);

    let params_gen = generics.params_generator(generics::ImplementationGeneric(true));
    let args_gen = generics.arguments_generator();

    // Where bounds on the entire impl block,
    // TODO: Is it correct to always use `Sync` in here here?
    // It must be for Async at least?
    let impl_where_bounds = match &generics.deps {
        generics::Deps::Generic { trait_bounds, .. } => {
            let impl_trait_bounds = if trait_bounds.is_empty() {
                None
            } else {
                Some(quote! {
                    ::entrait::Impl<EntraitT>: #(#trait_bounds)+*,
                })
            };

            let standard_bounds = if input_fn.use_associated_future(attr) {
                // Deps must be 'static for zero-cost futures to work
                quote! { Sync + 'static }
            } else {
                quote! { Sync }
            };

            quote_spanned! { span=>
                where #impl_trait_bounds EntraitT: #standard_bounds
            }
        }
        generics::Deps::Concrete(_) => quote_spanned! { span=>
            where EntraitT: #trait_ident #args_gen + Sync
        },
        generics::Deps::NoDeps => quote_spanned! { span=>
            where EntraitT: Sync
        },
    };

    let associated_fut_impl = &entrait_sig.associated_fut_impl;

    let generic_fn_def = gen_delegating_fn_item(
        span,
        input_fn,
        &input_fn_ident,
        entrait_sig,
        match &generics.deps {
            generics::Deps::Generic { .. } => FnReceiverKind::SelfArg,
            generics::Deps::Concrete(_) => FnReceiverKind::SelfAsRefReceiver,
            generics::Deps::NoDeps => FnReceiverKind::RefSelfArg,
        },
        &generics.deps,
    )?;

    let generic_impl_block = quote_spanned! { span=>
        #async_trait_attribute
        impl #params_gen #trait_ident #args_gen for ::entrait::Impl<EntraitT> #impl_where_bounds {
            #associated_fut_impl
            #generic_fn_def
        }
    };

    Ok(match &generics.deps {
        generics::Deps::Concrete(path) => {
            let concrete_fn_def = gen_delegating_fn_item(
                span,
                input_fn,
                &input_fn_ident,
                entrait_sig,
                FnReceiverKind::SelfArg,
                &generics.deps,
            )?;

            let params_gen = generics.params_generator(generics::ImplementationGeneric(false));

            quote_spanned! { span=>
                #generic_impl_block

                // Specific impl for the concrete type:
                #async_trait_attribute
                impl #params_gen #trait_ident #args_gen for #path {
                    #associated_fut_impl
                    #concrete_fn_def
                }
            }
        }
        _ => generic_impl_block,
    })
}

fn gen_delegating_fn_item(
    span: Span,
    input_fn: &InputFn,
    fn_ident: &syn::Ident,
    entrait_sig: &EntraitSignature,
    receiver_kind: FnReceiverKind,
    deps: &generics::Deps,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut opt_dot_await = input_fn.opt_dot_await(span);
    let trait_fn_sig = &entrait_sig.sig;

    let arguments = input_fn
        .fn_sig
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            if deps.is_deps_param(index) {
                match receiver_kind {
                    FnReceiverKind::SelfArg => Some(Ok(quote_spanned! { span=> self })),
                    FnReceiverKind::RefSelfArg => Some(Ok(quote_spanned! { span=> &self })),
                    FnReceiverKind::SelfAsRefReceiver => None,
                }
            } else {
                Some(match arg {
                    syn::FnArg::Receiver(_) => {
                        Err(syn::Error::new(arg.span(), "Unexpected receiver arg"))
                    }
                    syn::FnArg::Typed(pat_typed) => match pat_typed.pat.as_ref() {
                        syn::Pat::Ident(pat_ident) => {
                            let ident = &pat_ident.ident;
                            Ok(quote_spanned! { span=> #ident })
                        }
                        _ => Err(syn::Error::new(
                            arg.span(),
                            "Expected ident for function argument",
                        )),
                    },
                })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    let function_call = match receiver_kind {
        FnReceiverKind::SelfAsRefReceiver => quote_spanned! { span=>
            self.as_ref().#fn_ident(#(#arguments),*)
        },
        _ => quote_spanned! { span=>
            #fn_ident(#(#arguments),*)
        },
    };

    if entrait_sig.associated_fut_decl.is_some() {
        opt_dot_await = None;
    }

    Ok(quote_spanned! { span=>
        #trait_fn_sig {
            #function_call #opt_dot_await
        }
    })
}

impl EntraitAttr {
    pub fn opt_unimock_attribute(
        &self,
        input_fn: &InputFn,
        deps: &generics::Deps,
    ) -> Option<proc_macro2::TokenStream> {
        match self.default_option(self.unimock, false) {
            SpanOpt(true, span) => {
                let fn_ident = &input_fn.fn_sig.ident;

                let unmocked = match deps {
                    generics::Deps::Generic { .. } => quote! { #fn_ident },
                    generics::Deps::Concrete(_) => quote! { _ },
                    generics::Deps::NoDeps => {
                        let arguments =
                            input_fn
                                .fn_sig
                                .inputs
                                .iter()
                                .filter_map(|fn_arg| match fn_arg {
                                    syn::FnArg::Receiver(_) => None,
                                    syn::FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
                                        syn::Pat::Ident(pat_ident) => Some(&pat_ident.ident),
                                        _ => None,
                                    },
                                });

                        quote! { #fn_ident(#(#arguments),*) }
                    }
                };

                Some(self.gated_mock_attr(span, quote_spanned! {span=>
                    ::entrait::__unimock::unimock(prefix=::entrait::__unimock, mod=#fn_ident, as=[Fn], unmocked=[#unmocked])
                }))
            }
            _ => None,
        }
    }

    pub fn opt_mockall_automock_attribute(&self) -> Option<proc_macro2::TokenStream> {
        match self.default_option(self.mockall, false) {
            SpanOpt(true, span) => {
                Some(self.gated_mock_attr(span, quote_spanned! { span=> ::mockall::automock }))
            }
            _ => None,
        }
    }

    fn gated_mock_attr(&self, span: Span, attr: TokenStream) -> TokenStream {
        match self.export_value() {
            true => quote_spanned! {span=>
                #[#attr]
            },
            false => quote_spanned! {span=>
                #[cfg_attr(test, #attr)]
            },
        }
    }
}

enum FnReceiverKind {
    /// f(self, ..)
    SelfArg,
    /// f(&self, ..)
    RefSelfArg,
    /// self.as_ref().f(..)
    SelfAsRefReceiver,
}

impl InputFn {
    fn opt_dot_await(&self, span: Span) -> Option<proc_macro2::TokenStream> {
        if let Some(_) = self.fn_sig.asyncness {
            Some(quote_spanned! { span=> .await })
        } else {
            None
        }
    }

    pub fn use_associated_future(&self, attr: &EntraitAttr) -> bool {
        match (attr.async_strategy(), self.fn_sig.asyncness) {
            (SpanOpt(AsyncStrategy::AssociatedFuture, _), Some(_async)) => true,
            _ => false,
        }
    }

    fn opt_async_trait_attribute(&self, attr: &EntraitAttr) -> Option<proc_macro2::TokenStream> {
        match (attr.async_strategy(), self.fn_sig.asyncness) {
            (SpanOpt(AsyncStrategy::AsyncTrait, span), Some(_async)) => {
                Some(quote_spanned! { span=> #[::entrait::__async_trait::async_trait] })
            }
            _ => None,
        }
    }
}

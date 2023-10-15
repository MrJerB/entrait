mod issue_29 {
    use entrait_core::{
        entrait_impl,
        input::{Input, InputImpl},
    };
    use entrait_macros::entrait;
    use quote::{quote, quote_spanned};
    use syn::parse2;

    #[entrait(pub FooImpl, delegate_by = FooDelegate)]
    pub trait Foo {
        fn do_foo<'a>(&self, input: &'a str) -> &'a str;
    }

    struct MyFoo;

    #[test]
    fn issue_29() {
        let example = quote! {

            impl FooImpl for MyFoo {
                fn do_foo<'a, D>(deps: &D, input: &'a str) -> &'a str {
                    input
                }
            }
        };

        let input = parse2::<Input>(example).expect("");

        if let Input::Impl(input_impl) = input {
            let mut attr =
                parse2::<entrait_impl::input_attr::EntraitSimpleImplAttr>(quote!()).expect("");

            let after = entrait_impl::output_tokens_for_impl(attr, input_impl).expect("");
            println!("{}", after)
        }
    }
}

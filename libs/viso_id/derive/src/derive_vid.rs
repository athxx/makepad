use proc_macro::TokenStream;

use viso_proc_macro::{TokenBuilder, TokenParser};

pub fn derive_vid_impl(input: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();
    let mut parser = TokenParser::new(input);
    let _main_attribs = parser.eat_attributes();
    parser.eat_ident("pub");
    if parser.eat_ident("struct") {
        if let Some(struct_name) = parser.eat_any_ident() {
            tb.add("impl");
            tb.add("From<VID> for").ident(&struct_name).add("{");
            tb.add("    fn from(vid:VID)->")
                .ident(&struct_name)
                .add("{")
                .ident(&struct_name)
                .add("(vid)}");
            tb.add("}");

            tb.add("impl");
            tb.add("From<&[VID;1]> for").ident(&struct_name).add("{");
            tb.add("    fn from(vid:&[VID;1])->")
                .ident(&struct_name)
                .add("{")
                .ident(&struct_name)
                .add("(vid[0])}");
            tb.add("}");

            tb.add("impl");
            tb.add("From<u64> for").ident(&struct_name).add("{");
            tb.add("    fn from(vid:u64)->")
                .ident(&struct_name)
                .add("{")
                .ident(&struct_name)
                .add("(VID(vid))}");
            tb.add("}");

            return tb.end();
        }
    }
    parser.unexpected()
}

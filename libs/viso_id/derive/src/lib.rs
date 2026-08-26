use proc_macro::TokenStream;

use viso_proc_macro::{error, VID, TokenBuilder, TokenParser};

mod derive_vid;
use crate::derive_vid::*;

fn parse_ident(parser: &mut TokenParser) -> Result<String, TokenStream> {
    parser.expect_any_ident()
}

fn eat_ident(parser: &mut TokenParser) -> Option<String> {
    parser.eat_any_ident()
}

fn parse_single_token(item: TokenStream) -> String {
    let s = item.to_string();
    s.replace(' ', "")
}

#[proc_macro]
pub fn vid(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();
    let v = parse_single_token(item);
    let id = VID::from_str(&v);
    tb.add("VID (").suf_u64(id.0).add(")");
    tb.end()
}

#[proc_macro]
pub fn some_id(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();
    let v = parse_single_token(item);
    let id = VID::from_str(&v);
    tb.add("Some(VID (").suf_u64(id.0).add("))");
    tb.end()
}

#[proc_macro]
pub fn id(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();
    let v = parse_single_token(item);
    if !v.is_empty() {
        let id = VID::from_str(&v);
        tb.add("VID (").suf_u64(id.0).add(")");
    } else {
        tb.add("VID (0)");
    }
    tb.end()
}

#[proc_macro]
pub fn ids(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();
    let mut parser = TokenParser::new(item);
    fn parse(parser: &mut TokenParser, tb: &mut TokenBuilder) -> Result<(), TokenStream> {
        tb.add("&[");
        loop {
            // if its a {} insert it as code
            if parser.open_paren() {
                tb.stream(Some(parser.eat_level()));
                tb.add(",");
            } else {
                let ident = parse_ident(parser)?;
                let id = VID::from_str(&ident);
                tb.add("VID (").suf_u64(id.0).add("),");
            }

            if parser.eat_eot() {
                tb.add("]");
                return Ok(());
            }
            parser.expect_punct_any('.')?
        }
    }
    if let Err(e) = parse(&mut parser, &mut tb) {
        return e;
    };
    tb.end()
}

fn ids_array_impl(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();
    let mut parser = TokenParser::new(item);
    fn parse(parser: &mut TokenParser, tb: &mut TokenBuilder) -> Result<(), TokenStream> {
        tb.add("&[");
        'outer: loop {
            tb.add("&[");
            loop {
                let ident = parse_ident(parser)?;
                let id = VID::from_str(&ident);
                tb.add("VID (").suf_u64(id.0).add("),");
                if parser.eat_eot() {
                    tb.add("]");
                    break 'outer;
                }
                if parser.eat_punct_alone(',') {
                    tb.add("]");
                    break;
                }
                parser.expect_punct_any('.')?
            }
            tb.add(",");
            if parser.eat_eot() {
                break;
            }
        }
        tb.add("]");
        Ok(())
    }
    if let Err(e) = parse(&mut parser, &mut tb) {
        return e;
    };
    tb.end()
}

#[proc_macro]
pub fn ids_array(item: TokenStream) -> TokenStream {
    ids_array_impl(item)
}

#[proc_macro]
pub fn ids_list(item: TokenStream) -> TokenStream {
    ids_array_impl(item)
}

// absolutely a very bad idea but lets see if we can do this.
#[proc_macro]
pub fn vid_num(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();

    let mut parser = TokenParser::new(item);
    if let Some(name) = eat_ident(&mut parser) {
        if !parser.eat_punct_alone(',') {
            return error("please add a number");
        }
        // then eat the next bit
        let arg = parser.eat_level();
        let id = VID::from_str(&name);
        tb.add("VID::from_num(")
            .suf_u64(id.0)
            .add(",")
            .stream(Some(arg))
            .add(")");
        tb.end()
    } else {
        parser.unexpected()
    }
}

#[proc_macro]
pub fn id_lut(item: TokenStream) -> TokenStream {
    let mut tb = TokenBuilder::new();

    let mut parser = TokenParser::new(item);
    if let Some(name) = eat_ident(&mut parser) {
        tb.add("VID::from_str_with_lut(")
            .string(&name)
            .add(").unwrap()");
        tb.end()
    } else if let Some(punct) = parser.eat_any_punct() {
        tb.add("VID::from_str_with_lut(")
            .string(&punct)
            .add(").unwrap()");
        tb.end()
    } else {
        parser.unexpected()
    }
}

#[proc_macro_derive(FromVID)]
pub fn derive_vid(input: TokenStream) -> TokenStream {
    derive_vid_impl(input)
}

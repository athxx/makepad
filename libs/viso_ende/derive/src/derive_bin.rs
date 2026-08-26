use viso_proc_macro::*;
use proc_macro::TokenStream;

pub fn derive_en_bin_impl(input: TokenStream) -> TokenStream {
    let mut ps = TokenParser::new(input);
    let mut tb = TokenBuilder::new();

    ps.eat_attributes();
    ps.eat_ident("pub");
    if ps.eat_ident("struct") {
        if let Some(name) = ps.eat_any_ident() {
            let generic = ps.eat_generic();
            let types = ps.eat_all_types();
            let where_clause = ps.eat_where_clause(Some("EnBin"));

            tb.add("impl").stream(generic.clone());
            tb.add("EnBin for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ fn en_bin ( & self , s : & mut Vec < u8 > ) {");

            if let Some(types) = types {
                for i in 0..types.len() {
                    tb.add("self .").unsuf_usize(i).add(". en_bin ( s ) ;");
                }
            } else if let Some(fields) = ps.eat_all_struct_fields() {
                for field in fields {
                    tb.add("self .").ident(&field.name).add(". en_bin ( s ) ;");
                }
            } else {
                return ps.unexpected();
            }
            tb.add("} } ;");
            return tb.end();
        }
    } else if ps.eat_ident("enum") {
        if let Some(name) = ps.eat_any_ident() {
            let generic = ps.eat_generic();
            let where_clause = ps.eat_where_clause(Some("EnBin"));

            tb.add("impl").stream(generic.clone());
            tb.add("EnBin for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ fn en_bin ( & self , s : & mut Vec < u8 > ) {");
            tb.add("match self {");

            if !ps.open_brace() {
                return ps.unexpected();
            }
            let mut index = 0;
            while !ps.eat_eot() {
                ps.eat_attributes();
                // parse ident
                if let Some(variant) = ps.eat_any_ident() {
                    if let Some(types) = ps.eat_all_types() {
                        tb.add("Self ::").ident(&variant).add("(");
                        for i in 0..types.len() {
                            tb.ident(&format!("n{}", i)).add(",");
                        }
                        tb.add(") => {").suf_u16(index).add(". en_bin ( s ) ;");
                        for i in 0..types.len() {
                            tb.ident(&format!("n{}", i)).add(". en_bin ( s ) ;");
                        }
                        tb.add("}");
                    } else if let Some(fields) = ps.eat_all_struct_fields() {
                        // named variant
                        tb.add("Self ::").ident(&variant).add("{");
                        for field in fields.iter() {
                            tb.ident(&field.name).add(",");
                        }
                        tb.add("} => {").suf_u16(index).add(". en_bin ( s ) ;");
                        for field in fields {
                            tb.ident(&field.name).add(". en_bin ( s ) ;");
                        }
                        tb.add("}");
                    } else if ps.is_punct_alone(',') || ps.is_eot() {
                        // bare variant
                        tb.add("Self ::").ident(&variant).add("=> {");
                        tb.suf_u16(index).add(". en_bin ( s ) ; }");
                    } else {
                        return ps.unexpected();
                    }
                    index += 1;
                    ps.eat_punct_alone(',');
                } else {
                    return ps.unexpected();
                }
            }
            tb.add("} } } ;");
            return tb.end();
        }
    }
    ps.unexpected()
}

pub fn derive_de_bin_impl(input: TokenStream) -> TokenStream {
    let mut ps = TokenParser::new(input);
    let mut tb = TokenBuilder::new();

    ps.eat_attributes();
    ps.eat_ident("pub");
    if ps.eat_ident("struct") {
        if let Some(name) = ps.eat_any_ident() {
            let generic = ps.eat_generic();
            let types = ps.eat_all_types();
            let where_clause = ps.eat_where_clause(Some("DeBin"));

            tb.add("impl").stream(generic.clone());
            tb.add("DeBin for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ fn de_bin ( o : & mut usize , d : & [ u8 ] )");
            tb.add("-> std :: result :: Result < Self , DeBinErr > { ");
            tb.add("std :: result :: Result :: Ok ( Self");

            if let Some(types) = types {
                tb.add("(");
                for _ in 0..types.len() {
                    tb.add("DeBin :: de_bin ( o , d ) ? ,");
                }
                tb.add(")");
            } else if let Some(fields) = ps.eat_all_struct_fields() {
                tb.add("{");
                for field in fields {
                    tb.ident(&field.name).add(": DeBin :: de_bin ( o , d ) ? ,");
                }
                tb.add("}");
            } else {
                return ps.unexpected();
            }
            tb.add(") } } ;");
            return tb.end();
        }
    } else if ps.eat_ident("enum") {
        if let Some(name) = ps.eat_any_ident() {
            let generic = ps.eat_generic();
            let where_clause = ps.eat_where_clause(Some("DeBin"));

            tb.add("impl").stream(generic.clone());
            tb.add("DeBin for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ fn de_bin ( o : & mut usize , d : & [ u8 ] )");
            tb.add("-> std :: result :: Result < Self , DeBinErr > {");
            tb.add("let id : u16 = DeBin :: de_bin ( o , d ) ? ;");
            tb.add("match id {");

            if !ps.open_brace() {
                return ps.unexpected();
            }
            let mut index = 0;
            while !ps.eat_eot() {
                // parse ident
                ps.eat_attributes();
                if let Some(variant) = ps.eat_any_ident() {
                    tb.suf_u16(index as u16).add("=> {");
                    tb.add("std :: result :: Result :: Ok ( Self ::");
                    if let Some(types) = ps.eat_all_types() {
                        tb.ident(&variant).add("(");
                        for _ in 0..types.len() {
                            tb.add("DeBin :: de_bin ( o , d ) ? ,");
                        }
                        tb.add(")");
                    } else if let Some(fields) = ps.eat_all_struct_fields() {
                        // named variant
                        tb.ident(&variant).add("{");
                        for field in fields.iter() {
                            tb.ident(&field.name).add(": DeBin :: de_bin ( o , d ) ? ,");
                        }
                        tb.add("}");
                    } else if ps.is_punct_alone(',') || ps.is_eot() {
                        // bare variant
                        tb.ident(&variant);
                    } else {
                        return ps.unexpected();
                    }

                    tb.add(") }");
                    index += 1;
                    ps.eat_punct_alone(',');
                } else {
                    return ps.unexpected();
                }
            }
            tb.add("_ => std :: result :: Result :: Err ( DeBinErr { o : * o , l :");
            tb.unsuf_usize(1)
                .add(", s : d . len ( ) , msg : ")
                .string(&name)
                .add(". to_string ( ) } )");
            tb.add("} } } ;");
            return tb.end();
        }
    }
    ps.unexpected()
}

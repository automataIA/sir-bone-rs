//! Opt-in, syntax-only inspection of Rust functions and calls.

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, File, ImplItem, Item, Type};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub calls: Vec<Call>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub name: String,
    pub line: usize,
    pub kind: &'static str,
}

/// Parse Rust syntax and collect functions and syntactic call sites.
///
/// This deliberately does not resolve names or expand macros.
pub fn parse(source: &str) -> Result<Vec<Function>, String> {
    let file: File = syn::parse_file(source).map_err(|err| err.to_string())?;
    let mut functions = Vec::new();
    collect_items(&file.items, source, &[], &mut functions);
    functions.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
    });
    Ok(functions)
}

fn collect_items(items: &[Item], source: &str, prefix: &[String], out: &mut Vec<Function>) {
    for item in items {
        match item {
            Item::Fn(item) => {
                let name = qualify(prefix, &item.sig.ident.to_string());
                out.push(collect_function(
                    &name,
                    &item.sig.fn_token.span,
                    Some(&item.block),
                    item.block.brace_token.span.close().end().line,
                ));
            }
            Item::Mod(item) => {
                if let Some((_, items)) = &item.content {
                    let mut nested = prefix.to_vec();
                    nested.push(item.ident.to_string());
                    collect_items(items, source, &nested, out);
                }
            }
            Item::Impl(item) => {
                let ty = qualify(prefix, &type_name(&item.self_ty, source));
                let trait_name = item
                    .trait_
                    .as_ref()
                    .map(|(_, path, _)| qualify(prefix, &path_name(path)));
                for associated in &item.items {
                    if let ImplItem::Fn(method) = associated {
                        let name = match &trait_name {
                            Some(trait_name) => {
                                format!("<{ty} as {trait_name}>::{}", method.sig.ident)
                            }
                            None => format!("{ty}::{}", method.sig.ident),
                        };
                        out.push(collect_function(
                            &name,
                            &method.sig.fn_token.span,
                            Some(&method.block),
                            method.block.brace_token.span.close().end().line,
                        ));
                    }
                }
            }
            Item::Trait(item) => {
                for associated in &item.items {
                    if let syn::TraitItem::Fn(method) = associated {
                        let name = format!(
                            "{}::{}",
                            qualify(prefix, &item.ident.to_string()),
                            method.sig.ident
                        );
                        let end_line = method.default.as_ref().map_or_else(
                            || method.sig.span().end().line,
                            |body| body.brace_token.span.close().end().line,
                        );
                        out.push(collect_function(
                            &name,
                            &method.sig.fn_token.span,
                            method.default.as_ref(),
                            end_line,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_function(
    name: &str,
    fn_span: &Span,
    body: Option<&syn::Block>,
    end_line: usize,
) -> Function {
    let mut visitor = CallVisitor::default();
    if let Some(body) = body {
        visitor.visit_block(body);
    }
    visitor.calls.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then(a.kind.cmp(b.kind))
            .then(a.name.cmp(&b.name))
    });
    visitor.calls.dedup();
    Function {
        name: name.to_owned(),
        start_line: fn_span.start().line,
        end_line,
        calls: visitor.calls,
    }
}

#[derive(Default)]
struct CallVisitor {
    calls: Vec<Call>,
}

impl<'ast> Visit<'ast> for CallVisitor {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        let (name, kind) = match &*node.func {
            Expr::Path(path) => (path_name(&path.path), "path"),
            _ => ("<indirect>".to_owned(), "indirect_unresolved"),
        };
        self.calls.push(Call {
            name,
            line: node.span().start().line,
            kind,
        });
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.calls.push(Call {
            name: node.method.to_string(),
            line: node.span().start().line,
            kind: "method_unresolved",
        });
        visit::visit_expr_method_call(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.calls.push(Call {
            name: format!("{}!", path_name(&node.path)),
            line: node.span().start().line,
            kind: "macro_unexpanded",
        });
        visit::visit_macro(self, node);
    }

    // Nested block items have distinct bodies and are deliberately excluded
    // here. Module, impl, and trait members are collected by `collect_items`.
    fn visit_item(&mut self, _node: &'ast Item) {}
}

fn qualify(prefix: &[String], leaf: &str) -> String {
    prefix
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(leaf))
        .collect::<Vec<_>>()
        .join("::")
}

fn path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn type_name(ty: &Type, source: &str) -> String {
    let span = ty.span();
    let start = source_offset(source, span.start().line, span.start().column);
    let end = source_offset(source, span.end().line, span.end().column);
    if let (Some(start), Some(end)) = (start, end) {
        if start < end && end <= source.len() {
            return source[start..end].trim().to_owned();
        }
    }
    format!("<type@{}:{}>", span.start().line, span.start().column)
}

fn source_offset(source: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    let line_start = if line == 1 {
        0
    } else {
        source.match_indices('\n').nth(line - 2)?.0 + 1
    };
    let line_text = source.get(line_start..)?;
    let line_text = &line_text[..line_text.find('\n').unwrap_or(line_text.len())];
    let byte_column = line_text.char_indices().nth(column).map_or_else(
        || (column == line_text.chars().count()).then_some(line_text.len()),
        |(i, _)| Some(i),
    )?;
    Some(line_start + byte_column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_modules_and_method_implementations() {
        let functions = parse(
            "mod left { fn same() {} } mod right { fn same() {} }\
             trait T { fn provided() { call(); } }\
             struct S; impl S { fn own(&self) {} }\
             impl T for S { fn provided() {} }\
             mod a { struct Same; trait Local { fn run(); } impl Same { fn call(&self) {} } impl Local for Same { fn run() {} } }\
             mod b { struct Same; trait Local { fn run(); } impl Same { fn call(&self) {} } impl Local for Same { fn run() {} } }",
        )
        .unwrap();
        let names: Vec<_> = functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"left::same"));
        assert!(names.contains(&"right::same"));
        assert!(names.contains(&"T::provided"));
        assert!(names.contains(&"S::own"));
        assert!(names.contains(&"<S as T>::provided"));
        assert!(names.contains(&"a::Same::call"));
        assert!(names.contains(&"b::Same::call"));
        assert!(names.contains(&"<a::Same as a::Local>::run"));
        assert!(names.contains(&"<b::Same as b::Local>::run"));
        assert!(names.contains(&"a::Local::run"));
        assert!(names.contains(&"b::Local::run"));

        let declaration = functions
            .iter()
            .find(|f| f.name == "a::Local::run")
            .unwrap();
        assert!(declaration.calls.is_empty());
    }

    #[test]
    fn calls_are_scoped_and_ignore_comments_and_strings() {
        let functions = parse(
            r#"fn outer() {
                inner();
                let _ = "fake()";
                // other_fake()
                fn nested() { nested_call(); }
            }
            fn other() { other_call(); }"#,
        )
        .unwrap();
        let outer = functions.iter().find(|f| f.name == "outer").unwrap();
        assert_eq!(
            outer
                .calls
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["inner"]
        );
        let other = functions.iter().find(|f| f.name == "other").unwrap();
        assert_eq!(other.calls[0].name, "other_call");
    }

    #[test]
    fn marks_methods_macros_and_indirect_calls() {
        let functions = parse("fn f() { (factory())(); thing.run(); println!(\"x\"); }").unwrap();
        let calls = &functions[0].calls;
        assert!(calls
            .iter()
            .any(|c| c.kind == "indirect_unresolved" && c.name == "<indirect>"));
        assert!(calls
            .iter()
            .any(|c| c.kind == "method_unresolved" && c.name == "run"));
        assert!(calls
            .iter()
            .any(|c| c.kind == "macro_unexpanded" && c.name == "println!"));
        assert!(calls
            .iter()
            .any(|c| c.kind == "path" && c.name == "factory"));
    }

    #[test]
    fn uses_body_end_lines_and_unicode_columns_for_type_names() {
        let source = "fn café() {} struct S; impl S {\n    fn own(&self) {\n    }\n}";
        let functions = parse(source).unwrap();
        let cafe = functions.iter().find(|f| f.name == "café").unwrap();
        assert_eq!((cafe.start_line, cafe.end_line), (1, 1));
        let method = functions.iter().find(|f| f.name == "S::own").unwrap();
        assert_eq!((method.start_line, method.end_line), (2, 3));
    }

    #[test]
    fn rejects_invalid_syntax() {
        assert!(parse("fn { nope").is_err());
    }
}

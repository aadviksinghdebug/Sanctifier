use soroban_sdk::Env;
use syn::{parse_str, File, Item, Type, Fields, Meta, ExprMethodCall, Macro};
use syn::visit::{self, Visit};
use syn::spanned::Spanned;
use serde::Serialize;
use thiserror::Error;
use std::collections::HashSet;

// ── Existing types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
pub struct SizeWarning {
    pub struct_name: String,
    pub estimated_size: usize,
    pub limit: usize,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub enum PatternType {
    Panic,
    Unwrap,
    Expect,
}

#[derive(Debug, Serialize)]
pub struct UnsafePattern {
    pub pattern_type: PatternType,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct PanicIssue {
    pub function_name: String,
    pub issue_type: String, // "panic!", "unwrap", "expect"
    pub location: String,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("invariant violation: {0}")]
    InvariantViolation(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub trait SanctifiedGuard {
    fn check_invariant(&self, env: &Env) -> Result<(), Error>;
}

// ── ArithmeticIssue (NEW) ─────────────────────────────────────────────────────

/// Represents an unchecked arithmetic operation that could overflow or underflow.
#[derive(Debug, Serialize, Clone)]
pub struct ArithmeticIssue {
    /// Contract function in which the operation was found.
    pub function_name: String,
    /// The operator: "+", "-", "*", "+=", "-=", "*=".
    pub operation: String,
    /// Human-readable suggestion pointing to the safe alternative.
    pub suggestion: String,
    /// "function_name:line" context string.
    pub location: String,
}

/// Unified finding for machine-readable (JSON) output.
#[derive(Debug, Serialize, Clone)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    pub line: usize,
    pub message: String,
}

// ── Analyzer ──────────────────────────────────────────────────────────────────
pub struct Analyzer {
    pub strict_mode: bool,
    pub ledger_limit: usize,
}

impl Analyzer {
    pub fn new(strict_mode: bool) -> Self {
        Self {
            strict_mode,
            ledger_limit: 64000, // Default 64 KB warning threshold
        }
    }

    pub fn scan_auth_gaps(&self, source: &str) -> Vec<String> {
        let file = match parse_str::<File>(source) {
            Ok(f) => f,
            Err(_) => return vec![],
        };

        let mut gaps = Vec::new();

        for item in file.items {
            if let Item::Impl(i) = item {
                for impl_item in &i.items {
                    if let syn::ImplItem::Fn(f) = impl_item {
                        let fn_name = f.sig.ident.to_string();
                        let mut has_mutation = false;
                        let mut has_auth = false;
                        self.check_fn_body(&f.block, &mut has_mutation, &mut has_auth);
                        if has_mutation && !has_auth {
                            gaps.push(fn_name);
                        }
                    }
                }
            }
        }

        gaps
    }

    // ── Panic / unwrap / expect detection ────────────────────────────────────

    /// Returns all `panic!`, `.unwrap()`, and `.expect()` calls found inside
    /// contract impl functions. Prefer returning `Result` instead.
    pub fn scan_panics(&self, source: &str) -> Vec<PanicIssue> {
        let file = match parse_str::<File>(source) {
            Ok(f) => f,
            Err(_) => return vec![],
        };

        let mut issues = Vec::new();

        for item in file.items {
            if let Item::Impl(i) = item {
                for impl_item in &i.items {
                    if let syn::ImplItem::Fn(f) = impl_item {
                        let fn_name = f.sig.ident.to_string();
                        self.check_fn_panics(&f.block, &fn_name, &mut issues);
                    }
                }
            }
        }

        issues
    }

    fn check_fn_panics(&self, block: &syn::Block, fn_name: &str, issues: &mut Vec<PanicIssue>) {
        for stmt in &block.stmts {
            match stmt {
                syn::Stmt::Expr(expr, _) => self.check_expr_panics(expr, fn_name, issues),
                syn::Stmt::Local(local) => {
                    if let Some(init) = &local.init {
                        self.check_expr_panics(&init.expr, fn_name, issues);
                    }
                }
                syn::Stmt::Macro(m) => {
                    if m.mac.path.is_ident("panic") {
                        issues.push(PanicIssue {
                            function_name: fn_name.to_string(),
                            issue_type: "panic!".to_string(),
                            location: fn_name.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn check_expr_panics(&self, expr: &syn::Expr, fn_name: &str, issues: &mut Vec<PanicIssue>) {
        match expr {
            syn::Expr::Macro(m) => {
                if m.mac.path.is_ident("panic") {
                    issues.push(PanicIssue {
                        function_name: fn_name.to_string(),
                        issue_type: "panic!".to_string(),
                        location: fn_name.to_string(),
                    });
                }
            }
            syn::Expr::MethodCall(m) => {
                let method_name = m.method.to_string();
                if method_name == "unwrap" || method_name == "expect" {
                    issues.push(PanicIssue {
                        function_name: fn_name.to_string(),
                        issue_type: method_name,
                        location: fn_name.to_string(),
                    });
                }
                self.check_expr_panics(&m.receiver, fn_name, issues);
                for arg in &m.args {
                    self.check_expr_panics(arg, fn_name, issues);
                }
            }
            syn::Expr::Call(c) => {
                for arg in &c.args {
                    self.check_expr_panics(arg, fn_name, issues);
                }
            }
            syn::Expr::Block(b) => self.check_fn_panics(&b.block, fn_name, issues),
            syn::Expr::If(i) => {
                self.check_expr_panics(&i.cond, fn_name, issues);
                self.check_fn_panics(&i.then_branch, fn_name, issues);
                if let Some((_, else_expr)) = &i.else_branch {
                    self.check_expr_panics(else_expr, fn_name, issues);
                }
            }
            syn::Expr::Match(m) => {
                self.check_expr_panics(&m.expr, fn_name, issues);
                for arm in &m.arms {
                    self.check_expr_panics(&arm.body, fn_name, issues);
                }
            }
            _ => {}
        }
    }

    // ── Mutation / auth helpers ───────────────────────────────────────────────

    fn check_fn_body(&self, block: &syn::Block, has_mutation: &mut bool, has_auth: &mut bool) {
        for stmt in &block.stmts {
            match stmt {
                syn::Stmt::Expr(expr, _) => self.check_expr(expr, has_mutation, has_auth),
                syn::Stmt::Local(local) => {
                    if let Some(init) = &local.init {
                        self.check_expr(&init.expr, has_mutation, has_auth);
                    }
                }
                _ => {}
            }
        }
    }

    fn check_expr(&self, expr: &syn::Expr, has_mutation: &mut bool, has_auth: &mut bool) {
        match expr {
            syn::Expr::Call(c) => {
                if let syn::Expr::Path(p) = &*c.func {
                    if let Some(segment) = p.path.segments.last() {
                        let ident = segment.ident.to_string();
                        if ident == "require_auth" || ident == "require_auth_for_args" {
                            *has_auth = true;
                        }
                    }
                }
                for arg in &c.args {
                    self.check_expr(arg, has_mutation, has_auth);
                }
            }
            syn::Expr::MethodCall(m) => {
                let method_name = m.method.to_string();
                if method_name == "set" || method_name == "update" || method_name == "remove" {
                    // Heuristic: check if receiver chain contains "storage"
                    let receiver_str = quote::quote!(#m.receiver).to_string();
                    if receiver_str.contains("storage") {
                        *has_mutation = true;
                    }
                }
                if method_name == "require_auth" || method_name == "require_auth_for_args" {
                    *has_auth = true;
                }
                self.check_expr(&m.receiver, has_mutation, has_auth);
                for arg in &m.args {
                    self.check_expr(arg, has_mutation, has_auth);
                }
            }
            syn::Expr::Block(b) => self.check_fn_body(&b.block, has_mutation, has_auth),
            syn::Expr::If(i) => {
                self.check_expr(&i.cond, has_mutation, has_auth);
                self.check_fn_body(&i.then_branch, has_mutation, has_auth);
                if let Some((_, else_expr)) = &i.else_branch {
                    self.check_expr(else_expr, has_mutation, has_auth);
                }
            }
            syn::Expr::Match(m) => {
                self.check_expr(&m.expr, has_mutation, has_auth);
                for arm in &m.arms {
                    self.check_expr(&arm.body, has_mutation, has_auth);
                }
            }
            _ => {}
        }
    }

    // ── Storage collision (stub) ──────────────────────────────────────────────

    pub fn check_storage_collisions(&self, _keys: Vec<String>) -> bool {
        false
    }

    // ── Ledger size analysis ──────────────────────────────────────────────────

    /// Warns about `#[contracttype]` structs whose estimated size exceeds the
    /// ledger entry limit.
    pub fn analyze_ledger_size(&self, source: &str) -> Vec<SizeWarning> {
        let file = match parse_str::<File>(source) {
            Ok(f) => f,
            Err(_) => return vec![],
        };

        let mut warnings = Vec::new();

        for item in file.items {
            match item {
                Item::Struct(s) => {
                    let has_contracttype = s.attrs.iter().any(|attr| {
                        matches!(&attr.meta, Meta::Path(path) if path.is_ident("contracttype"))
                    });

                    if has_contracttype {
                        let size = self.estimate_struct_size(&s);
                        if size > self.ledger_limit
                            || (self.strict_mode && size > self.ledger_limit / 2)
                        {
                            warnings.push(SizeWarning {
                                struct_name: s.ident.to_string(),
                                estimated_size: size,
                                limit: self.ledger_limit,
                            });
                        }
                    }
                }
                Item::Impl(_) | Item::Macro(_) => {} // skip gracefully
                _ => {}
            }
        }

        warnings
    }

    // ── Unsafe-pattern visitor ────────────────────────────────────────────────

    /// Visitor-based scan for `panic!`, `.unwrap()`, `.expect()` with line
    /// numbers derived from proc-macro2 span locations.
    pub fn analyze_unsafe_patterns(&self, source: &str) -> Vec<UnsafePattern> {
        let file = match parse_str::<File>(source) {
            Ok(f) => f,
            Err(_) => return vec![],
        };

        let mut visitor = UnsafeVisitor {
            patterns: Vec::new(),
        };
        visitor.visit_file(&file);
        visitor.patterns
    }

    // ── Arithmetic overflow detection (NEW) ───────────────────────────────────

    /// Scans contract impl functions for unchecked arithmetic (`+`, `-`, `*`,
    /// `+=`, `-=`, `*=`) and suggests the corresponding `checked_*` or
    /// `saturating_*` alternatives.
    pub fn scan_arithmetic_overflow(&self, source: &str) -> Vec<ArithmeticIssue> {
        let file = match parse_str::<File>(source) {
            Ok(f) => f,
            Err(_) => return vec![],
        };

        let mut visitor = ArithVisitor {
            issues: Vec::new(),
            current_fn: None,
            seen: HashSet::new(),
        };
        visitor.visit_file(&file);
        visitor.issues
    }

    // ── Size estimation helpers ───────────────────────────────────────────────

    fn estimate_struct_size(&self, s: &syn::ItemStruct) -> usize {
        let mut total = 0;
        match &s.fields {
            Fields::Named(fields) => {
                for f in &fields.named {
                    total += self.estimate_type_size(&f.ty);
                }
            }
            Fields::Unnamed(fields) => {
                for f in &fields.unnamed {
                    total += self.estimate_type_size(&f.ty);
                }
            }
            Fields::Unit => {}
        }
        total
    }

    fn estimate_type_size(&self, ty: &Type) -> usize {
        match ty {
            Type::Path(tp) => {
                if let Some(seg) = tp.path.segments.last() {
                    match seg.ident.to_string().as_str() {
                        "u32" | "i32" | "bool" => 4,
                        "u64" | "i64" => 8,
                        "u128" | "i128" | "I128" | "U128" => 16,
                        "Address" => 32,
                        "Bytes" | "BytesN" | "String" | "Symbol" => 64,
                        "Vec" | "Map" => 128,
                        _ => 32,
                    }
                } else {
                    8
                }
            }
            _ => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_panic() {
        let source = r#"
            pub fn test() {
                panic!("error");
            }
        "#;
        let analyzer = Analyzer::new(false);
        let patterns = analyzer.analyze_unsafe_patterns(source);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].snippet, "panic!()");
    }

    #[test]
    fn test_find_unwrap_expect() {
        let source = r#"
            pub fn test() {
                let x: Option<i32> = None;
                x.unwrap();
                x.expect("msg");
            }
        "#;
        let analyzer = Analyzer::new(false);
        let patterns = analyzer.analyze_unsafe_patterns(source);
        assert_eq!(patterns.len(), 2);
    }
}

// ── UnsafeVisitor ─────────────────────────────────────────────────────────────

struct UnsafeVisitor {
    patterns: Vec<UnsafePattern>,
}

impl<'ast> Visit<'ast> for UnsafeVisitor {
    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        if node.mac.path.is_ident("panic") {
            let line = node
                .mac
                .path
                .get_ident()
                .map(|i| i.span().start().line)
                .unwrap_or(0);
            self.patterns.push(UnsafePattern {
                pattern_type: PatternType::Panic,
                line,
                snippet: "panic!()".to_string(),
            });
        }
        visit::visit_expr_macro(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        match method.as_str() {
            "unwrap" => {
                let line = node.method.span().start().line;
                self.patterns.push(UnsafePattern {
                    pattern_type: PatternType::Unwrap,
                    line,
                    snippet: ".unwrap()".to_string(),
                });
            }
            "expect" => {
                let line = node.method.span().start().line;
                self.patterns.push(UnsafePattern {
                    pattern_type: PatternType::Expect,
                    line,
                    snippet: ".expect()".to_string(),
                });
            }
            _ => {}
        }
        visit::visit_expr_method_call(self, node);
    }
}

// ── ArithVisitor ──────────────────────────────────────────────────────────────

struct ArithVisitor {
    issues: Vec<ArithmeticIssue>,
    /// Name of the function currently being visited.
    current_fn: Option<String>,
    /// De-duplicates issues: one per (function_name, operator) pair.
    seen: HashSet<(String, String)>,
}

impl ArithVisitor {
    /// Returns `(operator_str, suggestion_text)` for overflow-prone binary ops,
    /// or `None` for operators that cannot overflow (comparisons, bitwise, etc).
    fn classify_op(op: &syn::BinOp) -> Option<(&'static str, &'static str)> {
        match op {
            syn::BinOp::Add(_) => Some((
                "+",
                "Use `.checked_add(rhs)` or `.saturating_add(rhs)` to handle overflow",
            )),
            syn::BinOp::Sub(_) => Some((
                "-",
                "Use `.checked_sub(rhs)` or `.saturating_sub(rhs)` to handle underflow",
            )),
            syn::BinOp::Mul(_) => Some((
                "*",
                "Use `.checked_mul(rhs)` or `.saturating_mul(rhs)` to handle overflow",
            )),
            syn::BinOp::AddAssign(_) => Some((
                "+=",
                "Replace `a += b` with `a = a.checked_add(b).expect(\"overflow\")`",
            )),
            syn::BinOp::SubAssign(_) => Some((
                "-=",
                "Replace `a -= b` with `a = a.checked_sub(b).expect(\"underflow\")`",
            )),
            syn::BinOp::MulAssign(_) => Some((
                "*=",
                "Replace `a *= b` with `a = a.checked_mul(b).expect(\"overflow\")`",
            )),
            _ => None,
        }
    }
}

impl<'ast> Visit<'ast> for ArithVisitor {
    /// Track the current function when descending into an impl method.
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let prev = self.current_fn.take();
        self.current_fn = Some(node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.current_fn = prev;
    }

    /// Also handle top-level `fn` items (helper functions outside impls).
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let prev = self.current_fn.take();
        self.current_fn = Some(node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.current_fn = prev;
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if let Some(fn_name) = self.current_fn.clone() {
            if let Some((op_str, suggestion)) = Self::classify_op(&node.op) {
                // Skip concatenation of string literals (false positive for `+`)
                if !is_string_literal(&node.left) && !is_string_literal(&node.right) {
                    let key = (fn_name.clone(), op_str.to_string());
                    if !self.seen.contains(&key) {
                        self.seen.insert(key);
                        // Line number from the left operand's span
                        let line = node.left.span().start().line;
                        self.issues.push(ArithmeticIssue {
                            function_name: fn_name.clone(),
                            operation: op_str.to_string(),
                            suggestion: suggestion.to_string(),
                            location: format!("{}:{}", fn_name, line),
                        });
                    }
                }
            }
        }
        // Continue descending so nested binary ops are also checked
        visit::visit_expr_binary(self, node);
    }
}

/// Returns `true` if the expression is a string literal — used to avoid
/// false-positives on `+` for string concatenation (rare in no_std Soroban
/// but included for correctness).
fn is_string_literal(expr: &syn::Expr) -> bool {
    matches!(
        expr,
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(_),
            ..
        })
    )
}

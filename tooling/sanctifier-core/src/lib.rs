use soroban_sdk::Env;
use syn::{parse_str, File, Item, Type, Fields, Meta};
use syn::visit::{self, Visit};
use syn::spanned::Spanned;
use serde::{Serialize, Deserialize};
use thiserror::Error;
use std::collections::HashSet;
use std::panic::{self, AssertUnwindSafe};
use regex::Regex;

pub mod gas_estimator;
pub mod gas_report;

// ── Panic Guard ───────────────────────────────────────────────────────────────

/// Runs analysis logic inside a panic guard. Returns empty/default on panic,
/// e.g. when complex macros (contractimpl, etc.) cause AST parsing to fail.
fn with_panic_guard<T, F>(f: F) -> T
where
    F: FnOnce() -> T + panic::UnwindSafe,
    T: Default,
{
    panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_default()
}

// ── Configuration ─────────────────────────────────────────────────────────────

pub const DEFAULT_LEDGER_ENTRY_LIMIT: usize = 64 * 1024;
pub const DEFAULT_APPROACHING_THRESHOLD: f64 = 0.8;

/// User-defined regex-based rule. Defined in .sanctify.toml under [[rules]].
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CustomRule {
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SanctifyConfig {
    #[serde(default = "default_ignore_paths")]
    pub ignore_paths: Vec<String>,
    #[serde(default = "default_enabled_rules")]
    pub enabled_rules: Vec<String>,
    #[serde(default = "default_ledger_limit")]
    pub ledger_limit: usize,
    #[serde(default = "default_approaching_threshold")]
    pub approaching_threshold: f64,
    #[serde(default)]
    pub strict_mode: bool,
    /// Custom regex rules (field name "rules" in TOML).
    #[serde(default, alias = "custom_rules")]
    pub rules: Vec<CustomRule>,
}

fn default_ignore_paths() -> Vec<String> { vec!["target".to_string(), ".git".to_string()] }
fn default_enabled_rules() -> Vec<String> {
    vec!["auth_gaps".to_string(), "panics".to_string(), "arithmetic".to_string(), "ledger_size".to_string()]
}
fn default_ledger_limit() -> usize { DEFAULT_LEDGER_ENTRY_LIMIT }
fn default_approaching_threshold() -> f64 { DEFAULT_APPROACHING_THRESHOLD }

impl Default for SanctifyConfig {
    fn default() -> Self {
        Self {
            ignore_paths: default_ignore_paths(),
            enabled_rules: default_enabled_rules(),
            ledger_limit: default_ledger_limit(),
            approaching_threshold: default_approaching_threshold(),
            strict_mode: false,
            rules: vec![],
        }
    }
}

// ── Finding types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone, PartialEq)]
pub enum SizeWarningLevel { ExceedsLimit, ApproachingLimit }

#[derive(Debug, Serialize, Clone)]
pub struct SizeWarning {
    pub struct_name: String,
    pub estimated_size: usize,
    pub limit: usize,
    pub level: SizeWarningLevel,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq)]
pub enum PatternType { Panic, Unwrap, Expect }

#[derive(Debug, Serialize, Clone)]
pub struct UnsafePattern {
    pub pattern_type: PatternType,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct PanicIssue {
    pub function_name: String,
    pub issue_type: String,
    pub location: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ArithmeticIssue {
    pub function_name: String,
    pub operation: String,
    pub suggestion: String,
    pub location: String,
}

/// A match from a custom regex rule.
#[derive(Debug, Serialize, Clone)]
pub struct CustomRuleMatch {
    pub rule_name: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct GasEstimation {
    pub function_name: String,
    pub estimated_gas: u64,
    pub complexity_score: usize,
}

// ── Runtime Monitoring ────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum Error {
    #[error("invariant violation: {0}")]
    InvariantViolation(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Trait for runtime monitoring. Implement this to enforce invariants on your contract state.
pub trait SanctifiedGuard {
    fn check_invariant(&self, env: &Env) -> Result<(), Error>;
}

#[derive(Debug, Serialize, Clone)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    pub line: usize,
    pub message: String,
}

// ── Analyzer ──────────────────────────────────────────────────────────────────

pub struct Analyzer {
    pub config: SanctifyConfig,
}

impl Analyzer {
    pub fn new(config: SanctifyConfig) -> Self { Self { config } }

    pub fn analyze_custom_rules(&self, source: &str) -> Vec<CustomRuleMatch> {
        let mut matches = Vec::new();
        for rule in &self.config.rules {
            let re = match Regex::new(&rule.pattern) { Ok(r) => r, Err(_) => continue };
            for (line_no, line) in source.lines().enumerate() {
                if re.find(line).is_some() {
                    matches.push(CustomRuleMatch { rule_name: rule.name.clone(), line: line_no + 1, snippet: line.trim().to_string() });
                }
            }
        }
        matches
    }

    pub fn scan_auth_gaps(&self, source: &str) -> Vec<String> {
        with_panic_guard(|| self.scan_auth_gaps_impl(source))
    }

    fn scan_auth_gaps_impl(&self, source: &str) -> Vec<String> {
        let file = match parse_str::<File>(source) { Ok(f) => f, Err(_) => return vec![] };
        let mut gaps = Vec::new();
        for item in &file.items {
            if let Item::Impl(i) = item {
                for impl_item in &i.items {
                    if let syn::ImplItem::Fn(f) = impl_item {
                        if let syn::Visibility::Public(_) = f.vis {
                            let fn_name = f.sig.ident.to_string();
                            let mut has_mutation = false;
                            let mut has_auth = false;
                            self.check_fn_body(&f.block, &mut has_mutation, &mut has_auth);
                            if has_mutation && !has_auth { gaps.push(fn_name); }
                        }
                    }
                }
            }
        }
        gaps
    }

    pub fn scan_panics(&self, source: &str) -> Vec<PanicIssue> {
        with_panic_guard(|| self.scan_panics_impl(source))
    }

    fn scan_panics_impl(&self, source: &str) -> Vec<PanicIssue> {
        let file = match parse_str::<File>(source) { Ok(f) => f, Err(_) => return vec![] };
        let mut issues = Vec::new();
        for item in &file.items {
            if let Item::Impl(i) = item {
                for impl_item in &i.items {
                    if let syn::ImplItem::Fn(f) = impl_item {
                        self.check_fn_panics(&f.block, &f.sig.ident.to_string(), &mut issues);
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
                syn::Stmt::Local(local) => { if let Some(init) = &local.init { self.check_expr_panics(&init.expr, fn_name, issues); } }
                syn::Stmt::Macro(m) => { if m.mac.path.is_ident("panic") { issues.push(PanicIssue { function_name: fn_name.to_string(), issue_type: "panic!".to_string(), location: fn_name.to_string() }); } }
                _ => {}
            }
        }
    }

    fn check_expr_panics(&self, expr: &syn::Expr, fn_name: &str, issues: &mut Vec<PanicIssue>) {
        match expr {
            syn::Expr::Macro(m) => { if m.mac.path.is_ident("panic") { issues.push(PanicIssue { function_name: fn_name.to_string(), issue_type: "panic!".to_string(), location: fn_name.to_string() }); } }
            syn::Expr::MethodCall(m) => {
                let method_name = m.method.to_string();
                if method_name == "unwrap" || method_name == "expect" { issues.push(PanicIssue { function_name: fn_name.to_string(), issue_type: method_name, location: fn_name.to_string() }); }
                self.check_expr_panics(&m.receiver, fn_name, issues);
                for arg in &m.args { self.check_expr_panics(arg, fn_name, issues); }
            }
            syn::Expr::Call(c) => { for arg in &c.args { self.check_expr_panics(arg, fn_name, issues); } }
            syn::Expr::Block(b) => self.check_fn_panics(&b.block, fn_name, issues),
            syn::Expr::If(i) => {
                self.check_expr_panics(&i.cond, fn_name, issues);
                self.check_fn_panics(&i.then_branch, fn_name, issues);
                if let Some((_, else_expr)) = &i.else_branch { self.check_expr_panics(else_expr, fn_name, issues); }
            }
            syn::Expr::Match(m) => {
                self.check_expr_panics(&m.expr, fn_name, issues);
                for arm in &m.arms { self.check_expr_panics(&arm.body, fn_name, issues); }
            }
            _ => {}
        }
    }

    fn check_fn_body(&self, block: &syn::Block, has_mutation: &mut bool, has_auth: &mut bool) {
        for stmt in &block.stmts {
            match stmt {
                syn::Stmt::Expr(expr, _) => self.check_expr(expr, has_mutation, has_auth),
                syn::Stmt::Local(local) => { if let Some(init) = &local.init { self.check_expr(&init.expr, has_mutation, has_auth); } }
                syn::Stmt::Macro(m) => { if m.mac.path.is_ident("require_auth") || m.mac.path.is_ident("require_auth_for_args") { *has_auth = true; } }
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
                        if ident == "require_auth" || ident == "require_auth_for_args" { *has_auth = true; }
                    }
                }
                for arg in &c.args { self.check_expr(arg, has_mutation, has_auth); }
            }
            syn::Expr::MethodCall(m) => {
                let method_name = m.method.to_string();
                if method_name == "set" || method_name == "update" || method_name == "remove" {
                    let receiver_str = quote::quote!(#m.receiver).to_string();
                    if receiver_str.contains("storage") || receiver_str.contains("persistent") || receiver_str.contains("temporary") || receiver_str.contains("instance") { *has_mutation = true; }
                }
                if method_name == "require_auth" || method_name == "require_auth_for_args" { *has_auth = true; }
                self.check_expr(&m.receiver, has_mutation, has_auth);
                for arg in &m.args { self.check_expr(arg, has_mutation, has_auth); }
            }
            syn::Expr::Block(b) => self.check_fn_body(&b.block, has_mutation, has_auth),
            syn::Expr::If(i) => {
                self.check_expr(&i.cond, has_mutation, has_auth);
                self.check_fn_body(&i.then_branch, has_mutation, has_auth);
                if let Some((_, else_expr)) = &i.else_branch { self.check_expr(else_expr, has_mutation, has_auth); }
            }
            syn::Expr::Match(m) => {
                self.check_expr(&m.expr, has_mutation, has_auth);
                for arm in &m.arms { self.check_expr(&arm.body, has_mutation, has_auth); }
            }
            _ => {}
        }
    }

    pub fn check_storage_collisions(&self, _keys: Vec<String>) -> bool { false }

    pub fn analyze_ledger_size(&self, source: &str) -> Vec<SizeWarning> {
        with_panic_guard(|| self.analyze_ledger_size_impl(source))
    }

    fn analyze_ledger_size_impl(&self, source: &str) -> Vec<SizeWarning> {
        let file = match parse_str::<File>(source) { Ok(f) => f, Err(_) => return vec![] };
        let mut warnings = Vec::new();
        let limit = self.config.ledger_limit;
        let approaching = (limit as f64 * self.config.approaching_threshold) as usize;
        let strict = self.config.strict_mode;
        let strict_threshold = limit / 2;

        for item in &file.items {
            match item {
                Item::Struct(s) => {
                    if has_contracttype(&s.attrs) {
                        let size = self.estimate_struct_size(s);
                        if let Some(level) = classify_size(size, limit, approaching, strict, strict_threshold) {
                            warnings.push(SizeWarning { struct_name: s.ident.to_string(), estimated_size: size, limit, level });
                        }
                    }
                }
                Item::Enum(e) => {
                    if has_contracttype(&e.attrs) {
                        let size = self.estimate_enum_size(e);
                        if let Some(level) = classify_size(size, limit, approaching, strict, strict_threshold) {
                            warnings.push(SizeWarning { struct_name: e.ident.to_string(), estimated_size: size, limit, level });
                        }
                    }
                }
                _ => {}
            }
        }
        warnings
    }

    pub fn analyze_unsafe_patterns(&self, source: &str) -> Vec<UnsafePattern> {
        with_panic_guard(|| self.analyze_unsafe_patterns_impl(source))
    }

    fn analyze_unsafe_patterns_impl(&self, source: &str) -> Vec<UnsafePattern> {
        let file = match parse_str::<File>(source) { Ok(f) => f, Err(_) => return vec![] };
        let mut visitor = UnsafeVisitor { patterns: Vec::new() };
        visitor.visit_file(&file);
        visitor.patterns
    }

    pub fn scan_arithmetic_overflow(&self, source: &str) -> Vec<ArithmeticIssue> {
        with_panic_guard(|| self.scan_arithmetic_overflow_impl(source))
    }

    fn scan_arithmetic_overflow_impl(&self, source: &str) -> Vec<ArithmeticIssue> {
        let file = match parse_str::<File>(source) { Ok(f) => f, Err(_) => return vec![] };
        let mut visitor = ArithVisitor { issues: Vec::new(), current_fn: None, seen: HashSet::new() };
        visitor.visit_file(&file);
        visitor.issues
    }

    pub fn scan_gas_estimation(&self, source: &str) -> Vec<GasEstimation> {
        let file = match parse_str::<File>(source) { Ok(f) => f, Err(_) => return vec![] };
        let mut estimator = gas_estimator::GasEstimator::new();
        let mut estimations = Vec::new();
        for item in &file.items {
            if let Item::Impl(i) = item {
                for impl_item in &i.items {
                    if let syn::ImplItem::Fn(f) = impl_item {
                        if let syn::Visibility::Public(_) = f.vis {
                            let fn_name = f.sig.ident.to_string();
                            let gas = estimator.estimate_gas(&f.block);
                            estimations.push(GasEstimation { function_name: fn_name, estimated_gas: gas, complexity_score: 0 });
                        }
                    }
                }
            }
        }
        estimations
    }

    fn estimate_enum_size(&self, e: &syn::ItemEnum) -> usize {
        const DISCRIMINANT_SIZE: usize = 4;
        let mut max_variant = 0usize;
        for v in &e.variants {
            let mut variant_size = 0;
            match &v.fields {
                Fields::Named(fields) => { for f in &fields.named { variant_size += self.estimate_type_size(&f.ty); } }
                Fields::Unnamed(fields) => { for f in &fields.unnamed { variant_size += self.estimate_type_size(&f.ty); } }
                Fields::Unit => {}
            }
            max_variant = max_variant.max(variant_size);
        }
        DISCRIMINANT_SIZE + max_variant
    }

    fn estimate_struct_size(&self, s: &syn::ItemStruct) -> usize {
        let mut total = 0;
        match &s.fields {
            Fields::Named(fields) => { for f in &fields.named { total += self.estimate_type_size(&f.ty); } }
            Fields::Unnamed(fields) => { for f in &fields.unnamed { total += self.estimate_type_size(&f.ty); } }
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
                        "Vec" => { if let syn::PathArguments::AngleBracketed(args) = &seg.arguments { if let Some(syn::GenericArgument::Type(inner)) = args.args.first() { return 8 + self.estimate_type_size(inner); } } 128 }
                        "Map" => { if let syn::PathArguments::AngleBracketed(args) = &seg.arguments { let inner: usize = args.args.iter().filter_map(|a| if let syn::GenericArgument::Type(t) = a { Some(self.estimate_type_size(t)) } else { None }).sum(); if inner > 0 { return 16 + inner * 2; } } 128 }
                        "Option" => { if let syn::PathArguments::AngleBracketed(args) = &seg.arguments { if let Some(syn::GenericArgument::Type(inner)) = args.args.first() { return 1 + self.estimate_type_size(inner); } } 32 }
                        _ => 32,
                    }
                } else { 8 }
            }
            Type::Array(arr) => { if let syn::Expr::Lit(expr_lit) = &arr.len { if let syn::Lit::Int(lit) = &expr_lit.lit { if let Ok(n) = lit.base10_parse::<usize>() { return n * self.estimate_type_size(&arr.elem); } } } 64 }
            _ => 8,
        }
    }
}

fn has_contracttype(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| if let Meta::Path(path) = &attr.meta { path.is_ident("contracttype") || path.segments.iter().any(|s| s.ident == "contracttype") } else { false })
}

fn classify_size(size: usize, limit: usize, approaching: usize, strict: bool, strict_threshold: usize) -> Option<SizeWarningLevel> {
    if size > limit { Some(SizeWarningLevel::ExceedsLimit) } else if size > approaching || (strict && size > strict_threshold) { Some(SizeWarningLevel::ApproachingLimit) } else { None }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_with_limit() {
        let mut config = SanctifyConfig::default();
        config.ledger_limit = 50;
        let analyzer = Analyzer::new(config);
        let source = r#"
            #[contracttype]
            pub struct ExceedsLimit {
                pub buffer: Bytes,
            }
        "#;
        let warnings = analyzer.analyze_ledger_size(source);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].struct_name, "ExceedsLimit");
        assert_eq!(warnings[0].level, SizeWarningLevel::ExceedsLimit);
    }

    #[test]
    fn test_complex_macro_no_panic() {
        let analyzer = Analyzer::new(SanctifyConfig::default());
        let source = r#"
            #[contractimpl]
            impl Contract {
                pub fn test(_env: Env) {
                    let _x = 1u32;
                }
            }
        "#;
        // Must not panic
        let _ = analyzer.analyze_ledger_size(source);
        let _ = analyzer.scan_auth_gaps(source);
    }

    #[test]
    fn test_scan_auth_gaps() {
        let analyzer = Analyzer::new(SanctifyConfig::default());
        let source = r#"
            #[contractimpl]
            impl MyContract {
                pub fn set_data(env: Env, val: u32) {
                    env.storage().instance().set(&DataKey::Val, &val);
                }
            }
        "#;
        let gaps = analyzer.scan_auth_gaps(source);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0], "set_data");
    }
}

// ── Visitors ──────────────────────────────────────────────────────────────────

struct UnsafeVisitor {
    patterns: Vec<UnsafePattern>,
}

impl<'ast> Visit<'ast> for UnsafeVisitor {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if node.path.is_ident("panic") {
            let line = node.path.get_ident().map(|i| i.span().start().line).unwrap_or(0);
            self.patterns.push(UnsafePattern { pattern_type: PatternType::Panic, line, snippet: "panic!()".to_string() });
        }
        visit::visit_macro(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        if method == "unwrap" || method == "expect" {
            let line = node.method.span().start().line;
            let pattern_type = if method == "unwrap" { PatternType::Unwrap } else { PatternType::Expect };
            self.patterns.push(UnsafePattern { pattern_type, line, snippet: format!(".{}()", method) });
        }
        visit::visit_expr_method_call(self, node);
    }
}

struct ArithVisitor {
    issues: Vec<ArithmeticIssue>,
    current_fn: Option<String>,
    seen: HashSet<(String, String)>,
}

impl ArithVisitor {
    fn classify_op(op: &syn::BinOp) -> Option<(&'static str, &'static str)> {
        match op {
            syn::BinOp::Add(_) => Some(("+", "Use `.checked_add(rhs)` or `.saturating_add(rhs)`")),
            syn::BinOp::Sub(_) => Some(("-", "Use `.checked_sub(rhs)` or `.saturating_sub(rhs)`")),
            syn::BinOp::Mul(_) => Some(("*", "Use `.checked_mul(rhs)` or `.saturating_mul(rhs)`")),
            syn::BinOp::AddAssign(_) => Some(("+=", "Replace with `a = a.checked_add(b).expect(\"overflow\")`")),
            syn::BinOp::SubAssign(_) => Some(("-=", "Replace with `a = a.checked_sub(b).expect(\"underflow\")`")),
            syn::BinOp::MulAssign(_) => Some(("*=", "Replace with `a = a.checked_mul(b).expect(\"overflow\")`")),
            _ => None,
        }
    }
}

impl<'ast> Visit<'ast> for ArithVisitor {
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let prev = self.current_fn.take();
        self.current_fn = Some(node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.current_fn = prev;
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let prev = self.current_fn.take();
        self.current_fn = Some(node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.current_fn = prev;
    }
    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if let Some(fn_name) = self.current_fn.clone() {
            if let Some((op_str, suggestion)) = Self::classify_op(&node.op) {
                if !is_string_literal(&node.left) && !is_string_literal(&node.right) {
                    let key = (fn_name.clone(), op_str.to_string());
                    if !self.seen.contains(&key) {
                        self.seen.insert(key);
                        let line = node.left.span().start().line;
                        self.issues.push(ArithmeticIssue { function_name: fn_name.clone(), operation: op_str.to_string(), suggestion: suggestion.to_string(), location: format!("{}:{}", fn_name, line) });
                    }
                }
            }
        }
        visit::visit_expr_binary(self, node);
    }
}

fn is_string_literal(expr: &syn::Expr) -> bool {
    matches!(expr, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(_), .. }))
}

use clap::{Parser, Subcommand};
use colored::*;
use std::fs;
use std::path::{Path, PathBuf};
use sanctifier_core::{Analyzer, ArithmeticIssue, SizeWarning, UnsafePattern, PatternType, Finding};

#[derive(Parser)]
#[command(name = "sanctifier")]
#[command(about = "Stellar Soroban Security & Formal Verification Suite", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Analyze a Soroban contract for vulnerabilities
    Analyze {
        /// Path to the contract directory or Cargo.toml
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output format (text, json)
        #[arg(short, long, default_value = "text")]
        format: String,

        /// Limit for ledger entry size in bytes
        #[arg(short, long, default_value = "64000")]
        limit: usize,
    },
    /// Generate a security report
    Report {
        /// Output file path
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Initialize Sanctifier in a new project
    Init,
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Analyze {
            path,
            format,
            limit,
        } => {
            let is_json = format == "json";

            if !is_soroban_project(path) {
                eprintln!("{} Error: {:?} is not a valid Soroban project. (Missing Cargo.toml with 'soroban-sdk' dependency)", "❌".red(), path);
                std::process::exit(1);
            }

            // In JSON mode, send informational lines to stderr so stdout is clean JSON.
            if is_json {
                eprintln!(
                    "{} Sanctifier: Valid Soroban project found at {:?}",
                    "✨".green(),
                    path
                );
                eprintln!("{} Analyzing contract at {:?}...", "🔍".blue(), path);
            } else {
                println!(
                    "{} Sanctifier: Valid Soroban project found at {:?}",
                    "✨".green(),
                    path
                );
                println!("{} Analyzing contract at {:?}...", "🔍".blue(), path);
            }

            let mut analyzer = Analyzer::new(false);
            analyzer.ledger_limit = *limit;

            let mut all_size_warnings: Vec<SizeWarning> = Vec::new();
            let mut all_unsafe_patterns: Vec<UnsafePattern> = Vec::new();
            let mut all_auth_gaps: Vec<String> = Vec::new();
            let mut all_panic_issues = Vec::new();
            let mut all_arithmetic_issues: Vec<ArithmeticIssue> = Vec::new();

            if path.is_dir() {
                analyze_directory(
                    path,
                    &analyzer,
                    &mut all_size_warnings,
                    &mut all_unsafe_patterns,
                    &mut all_auth_gaps,
                    &mut all_panic_issues,
                    &mut all_arithmetic_issues,
                );
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                if let Ok(content) = fs::read_to_string(path) {
                    all_size_warnings.extend(analyzer.analyze_ledger_size(&content));

                    let patterns = analyzer.analyze_unsafe_patterns(&content);
                    for mut p in patterns {
                        p.snippet = format!("{}: {}", path.display(), p.snippet);
                        all_unsafe_patterns.push(p);
                    }
                    let gaps = analyzer.scan_auth_gaps(&content);
                    for g in gaps {
                        all_auth_gaps.push(format!("{}: {}", path.display(), g));
                    }
                    let panics = analyzer.scan_panics(&content);
                    for p in panics {
                        let mut p_mod = p.clone();
                        p_mod.location = format!("{}: {}", path.display(), p.location);
                        all_panic_issues.push(p_mod);
                    }

                    let arith = analyzer.scan_arithmetic_overflow(&content);
                    for mut a in arith {
                        a.location = format!("{}: {}", path.display(), a.location);
                        all_arithmetic_issues.push(a);
                    }
                    if !is_json {
                        println!("Analysis complete for file: {:?}", path);
                    }
                }
            }

            if is_json {
                eprintln!("{} Static analysis complete.", "✅".green());
            } else {
                println!("{} Static analysis complete.", "✅".green());
            }

            if is_json {
                let mut findings: Vec<Finding> = Vec::new();

                for w in &all_size_warnings {
                    let (file, struct_name) = match w.struct_name.split_once(": ") {
                        Some((f, s)) => (f.to_string(), s.to_string()),
                        None => (String::new(), w.struct_name.clone()),
                    };
                    findings.push(Finding {
                        severity: "warning".to_string(),
                        file,
                        line: 0,
                        message: format!(
                            "Struct '{}' approaching ledger entry size limit: estimated {} bytes (limit: {} bytes)",
                            struct_name, w.estimated_size, w.limit
                        ),
                    });
                }

                for p in &all_unsafe_patterns {
                    let (file, snippet) = match p.snippet.split_once(": ") {
                        Some((f, s)) => (f.to_string(), s.to_string()),
                        None => (String::new(), p.snippet.clone()),
                    };
                    findings.push(Finding {
                        severity: "warning".to_string(),
                        file,
                        line: p.line,
                        message: format!("Unsafe pattern detected: {}", snippet),
                    });
                }

                for gap in &all_auth_gaps {
                    let (file, func) = match gap.split_once(": ") {
                        Some((f, s)) => (f.to_string(), s.to_string()),
                        None => (String::new(), gap.clone()),
                    };
                    findings.push(Finding {
                        severity: "error".to_string(),
                        file,
                        line: 0,
                        message: format!(
                            "Function '{}' modifies state without require_auth()",
                            func
                        ),
                    });
                }

                for issue in &all_panic_issues {
                    let (file, _) = match issue.location.split_once(": ") {
                        Some((f, s)) => (f.to_string(), s.to_string()),
                        None => (String::new(), issue.location.clone()),
                    };
                    findings.push(Finding {
                        severity: "warning".to_string(),
                        file,
                        line: 0,
                        message: format!(
                            "Function '{}' uses '{}' — prefer returning Result or Error types",
                            issue.function_name, issue.issue_type
                        ),
                    });
                }

                for issue in &all_arithmetic_issues {
                    let (file, _) = match issue.location.split_once(": ") {
                        Some((f, s)) => (f.to_string(), s.to_string()),
                        None => (String::new(), issue.location.clone()),
                    };
                    findings.push(Finding {
                        severity: "warning".to_string(),
                        file,
                        line: 0,
                        message: format!(
                            "Function '{}': Unchecked `{}` operation. {}",
                            issue.function_name, issue.operation, issue.suggestion
                        ),
                    });
                }

                let json = serde_json::to_string_pretty(&findings).unwrap_or_else(|_| "[]".to_string());
                println!("{}", json);
            } else {
                if all_size_warnings.is_empty() && all_unsafe_patterns.is_empty() && all_auth_gaps.is_empty() && all_panic_issues.is_empty() && all_arithmetic_issues.is_empty() {
                    println!("No issues found.");
                } else {
                    for warning in &all_size_warnings {
                        println!(
                            "{} Warning: Struct {} is approaching ledger entry size limit!",
                            "⚠️".yellow(),
                            warning.struct_name.bold()
                        );
                        println!(
                            "   Estimated size: {} bytes (Limit: {} bytes)",
                            warning.estimated_size.to_string().red(),
                            warning.limit
                        );
                    }

                    for pattern in &all_unsafe_patterns {
                        let msg = match pattern.pattern_type {
                            PatternType::Panic => "Explicit panic!() call detected".red(),
                            PatternType::Unwrap => "unwrap() call detected".yellow(),
                            PatternType::Expect => "expect() call detected".yellow(),
                        };
                        println!(
                            "{} {}: {}",
                            "🚨".red(),
                            msg,
                            format!("{}:{}", pattern.line, pattern.snippet).bold()
                        );
                    }

                    if !all_auth_gaps.is_empty() {
                        println!("\n{} Found potential Authentication Gaps!", "🛑".red());
                        for gap in &all_auth_gaps {
                            println!("   {} Function {} is modifying state without require_auth()", "->".red(), gap.bold());
                        }
                    }

                    if !all_panic_issues.is_empty() {
                        println!("\n{} Found explicit Panics/Unwraps!", "🛑".red());
                        for issue in &all_panic_issues {
                            println!(
                                "   {} Function {}: Using {} (Location: {})",
                                "->".red(),
                                issue.function_name.bold(),
                                issue.issue_type.yellow().bold(),
                                issue.location
                            );
                        }
                    }

                    if !all_arithmetic_issues.is_empty() {
                        println!("\n{} Found unchecked Arithmetic Operations!", "🔢".yellow());
                        for issue in &all_arithmetic_issues {
                            println!(
                                "   {} Function {}: Unchecked `{}` ({})",
                                "->".red(),
                                issue.function_name.bold(),
                                issue.operation.yellow().bold(),
                                issue.location
                            );
                            println!("      {} {}", "💡".blue(), issue.suggestion);
                        }
                    }
                }
            }
        }
        Commands::Report { output } => {
            println!("{} Generating report...", "📄".yellow());
            if let Some(p) = output {
                println!("Report saved to {:?}", p);
            } else {
                println!("Report printed to stdout.");
            }
        }
        Commands::Init => {
            println!("{} Initializing Sanctifier configuration...", "⚙️".cyan());
            println!("Created .sanctify.toml");
        }
    }
}

fn is_soroban_project(path: &Path) -> bool {
    let cargo_toml_path = if path.is_dir() {
        path.join("Cargo.toml")
    } else if path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
        path.to_path_buf()
    } else {
        // If it's a .rs file, look for Cargo.toml in parent directories
        let mut current = path.parent();
        let mut found = None;
        while let Some(p) = current {
            let cargo = p.join("Cargo.toml");
            if cargo.exists() {
                found = Some(cargo);
                break;
            }
            current = p.parent();
        }
        match found {
            Some(f) => f,
            None => return false,
        }
    };

    if !cargo_toml_path.exists() {
        return false;
    }

    if let Ok(content) = fs::read_to_string(cargo_toml_path) {
        content.contains("soroban-sdk")
    } else {
        false
    }
}

fn analyze_directory(
    dir: &Path,
    analyzer: &Analyzer,
    all_size_warnings: &mut Vec<SizeWarning>,
    all_unsafe_patterns: &mut Vec<UnsafePattern>,
    all_auth_gaps: &mut Vec<String>,
    all_panic_issues: &mut Vec<sanctifier_core::PanicIssue>,
    all_arithmetic_issues: &mut Vec<ArithmeticIssue>,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                analyze_directory(
                    &path,
                    analyzer,
                    all_size_warnings,
                    all_unsafe_patterns,
                    all_auth_gaps,
                    all_panic_issues,
                    all_arithmetic_issues,
                );
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                if let Ok(content) = fs::read_to_string(&path) {
                    let warnings = analyzer.analyze_ledger_size(&content);
                    for mut w in warnings {
                        w.struct_name = format!("{}: {}", path.display(), w.struct_name);
                        all_size_warnings.push(w);
                    }

                    let patterns = analyzer.analyze_unsafe_patterns(&content);
                    for mut p in patterns {
                        p.snippet = format!("{}: {}", path.display(), p.snippet);
                        all_unsafe_patterns.push(p);
                    }

                    let gaps = analyzer.scan_auth_gaps(&content);
                    for g in gaps {
                        all_auth_gaps.push(format!("{}: {}", path.display(), g));
                    }

                    let panics = analyzer.scan_panics(&content);
                    for p in panics {
                        let mut p_mod = p.clone();
                        p_mod.location = format!("{}: {}", path.display(), p.location);
                        all_panic_issues.push(p_mod);
                    }

                    let arith = analyzer.scan_arithmetic_overflow(&content);
                    for mut a in arith {
                        a.location = format!("{}: {}", path.display(), a.location);
                        all_arithmetic_issues.push(a);
                    }
                }
            }
        }
    }
}

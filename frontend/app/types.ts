export type Severity = "critical" | "high" | "medium" | "low";

export interface SizeWarning {
  code?: string;
  struct_name: string;
  estimated_size: number;
  limit: number;
  level?: "ExceedsLimit" | "ApproachingLimit";
}

export interface PanicIssue {
  code: string;
  function_name: string;
  issue_type: string;
  location: string;
}

export interface UnsafePattern {
  code?: string;
  pattern_type: "Panic" | "Unwrap" | "Expect";
  line: number;
  snippet: string;
}

export interface ArithmeticIssue {
  code: string;
  function_name: string;
  operation: string;
  suggestion: string;
  location: string;
}

export interface CustomRuleMatch {
  code?: string;
  rule_name: string;
  line: number;
  snippet: string;
}

export interface CallGraphReportEdge {
  caller: string;
  callee: string;
  file: string;
  line: number;
  contract_id_expr: string;
  function_expr?: string | null;
}

export interface AnalysisReport {
  size_warnings?: SizeWarning[];
  unsafe_patterns?: UnsafePattern[];
  auth_gaps?: Array<string | { code: string; function_name: string }>;
  panic_issues?: PanicIssue[];
  arithmetic_issues?: ArithmeticIssue[];
  custom_rule_matches?: CustomRuleMatch[];
  call_graph?: CallGraphReportEdge[];
}

export interface Finding {
  id: string;
  code: string;
  severity: Severity;
  category: string;
  title: string;
  location: string;
  snippet?: string;
  line?: number;
  suggestion?: string;
  raw: unknown;
}

export interface CallGraphNode {
  id: string;
  label: string;
  type: "function" | "storage" | "external";
  file?: string;
  severity?: Severity;
}

export interface CallGraphEdge {
  source: string;
  target: string;
  label?: string;
  type: "calls" | "mutates" | "reads";
}

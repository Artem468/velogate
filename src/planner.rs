use crate::ast::*;
use lasso::Rodeo;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionPlan {
    pub endpoints: Vec<EndpointPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EndpointPlan {
    pub method: String,
    pub path: String,
    pub steps: Vec<PlannedStep>,
    pub layers: Vec<Vec<usize>>,
    pub response_reads: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedStep {
    pub index: usize,
    pub produces: String,
    pub kind: StepKind,
    pub reads: Vec<String>,
    pub dependencies: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Let,
    FetchHttp,
    CallGrpc,
    QueryDb,
    Pipe,
}

#[derive(Debug, Clone)]
pub enum PlanError {
    DuplicateVariable {
        endpoint: String,
        variable: String,
        first_step: usize,
        second_step: usize,
    },
    UndefinedVariable {
        endpoint: String,
        variable: String,
        used_by: String,
    },
    Cycle {
        endpoint: String,
    },
}

pub fn build_plan(ast: &FileAST, interner: &Rodeo) -> Result<ExecutionPlan, PlanError> {
    let globals = ast
        .gateway
        .static_dbs
        .iter()
        .map(|db| db.name)
        .collect::<HashSet<_>>();
    let endpoints = ast
        .endpoints
        .iter()
        .map(|endpoint| build_endpoint_plan(endpoint, interner, &globals))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ExecutionPlan { endpoints })
}

pub fn export_plan_dot(plan: &ExecutionPlan) -> String {
    let mut out = String::from("digraph velogate_plan {\n  rankdir=LR;\n");

    for (endpoint_idx, endpoint) in plan.endpoints.iter().enumerate() {
        let endpoint_node = format!("endpoint_{endpoint_idx}");
        out.push_str(&format!(
            "  {endpoint_node} [label=\"{} {}\", shape=oval];\n",
            dot_escape(&endpoint.method),
            dot_escape(&endpoint.path)
        ));

        for (layer_idx, layer) in endpoint.layers.iter().enumerate() {
            out.push_str(&format!(
                "  subgraph cluster_{endpoint_idx}_{layer_idx} {{\n    label=\"layer {layer_idx}\";\n"
            ));
            for step_idx in layer {
                let step = &endpoint.steps[*step_idx];
                out.push_str(&format!(
                    "    \"{endpoint_node}_step_{}\" [label=\"{}: {}\", shape=box];\n",
                    step.index,
                    step.index,
                    dot_escape(&step.produces)
                ));
            }
            out.push_str("  }\n");
        }

        for step in &endpoint.steps {
            out.push_str(&format!(
                "  {endpoint_node} -> \"{endpoint_node}_step_{}\";\n",
                step.index
            ));
            for dep in &step.dependencies {
                out.push_str(&format!(
                    "  \"{endpoint_node}_step_{dep}\" -> \"{endpoint_node}_step_{}\";\n",
                    step.index
                ));
            }
        }
    }

    out.push_str("}\n");
    out
}

fn build_endpoint_plan(
    endpoint: &Endpoint,
    interner: &Rodeo,
    globals: &HashSet<Sym>,
) -> Result<EndpointPlan, PlanError> {
    let endpoint_name = format!("{} {}", endpoint.method, endpoint.path);
    let mut produced_by = HashMap::<Sym, usize>::new();
    let mut steps = Vec::<PlannedStep>::with_capacity(endpoint.steps.len());

    for (index, step) in endpoint.steps.iter().enumerate() {
        let var = step_var(step);
        if let Some(first_step) = produced_by.insert(var, index) {
            return Err(PlanError::DuplicateVariable {
                endpoint: endpoint_name,
                variable: sym(interner, var),
                first_step,
                second_step: index,
            });
        }

        let reads = step_reads(step);
        steps.push(PlannedStep {
            index,
            produces: sym(interner, var),
            kind: step_kind(step),
            reads: reads.iter().map(|name| sym(interner, *name)).collect(),
            dependencies: Vec::new(),
        });
    }

    for (index, step) in endpoint.steps.iter().enumerate() {
        let reads = step_reads(step);
        let mut dependencies = BTreeSet::new();
        for read in reads {
            if globals.contains(&read) {
                continue;
            }

            let Some(dep_idx) = produced_by.get(&read).copied() else {
                return Err(PlanError::UndefinedVariable {
                    endpoint: endpoint_name,
                    variable: sym(interner, read),
                    used_by: steps[index].produces.clone(),
                });
            };

            dependencies.insert(dep_idx);
        }
        steps[index].dependencies = dependencies.into_iter().collect();
    }

    let response_reads = response_reads(endpoint);
    for read in &response_reads {
        if !globals.contains(read) && !produced_by.contains_key(read) {
            return Err(PlanError::UndefinedVariable {
                endpoint: endpoint_name,
                variable: sym(interner, *read),
                used_by: "respond".to_string(),
            });
        }
    }

    let layers = build_layers(&steps, endpoint)?;

    Ok(EndpointPlan {
        method: endpoint.method.clone(),
        path: endpoint.path.clone(),
        steps,
        layers,
        response_reads: response_reads
            .into_iter()
            .map(|name| sym(interner, name))
            .collect(),
    })
}

fn build_layers(steps: &[PlannedStep], endpoint: &Endpoint) -> Result<Vec<Vec<usize>>, PlanError> {
    let mut graph = DiGraph::<usize, ()>::new();
    let nodes = steps
        .iter()
        .map(|step| graph.add_node(step.index))
        .collect::<Vec<NodeIndex>>();

    for step in steps {
        for dep in &step.dependencies {
            graph.add_edge(nodes[*dep], nodes[step.index], ());
        }
    }

    let sorted = toposort(&graph, None).map_err(|_| PlanError::Cycle {
        endpoint: format!("{} {}", endpoint.method, endpoint.path),
    })?;

    let mut depth_by_step = BTreeMap::<usize, usize>::new();
    for node in sorted {
        let step_idx = graph[node];
        let step = &steps[step_idx];
        let depth = step
            .dependencies
            .iter()
            .filter_map(|dep| depth_by_step.get(dep))
            .max()
            .map_or(0, |depth| depth + 1);
        depth_by_step.insert(step_idx, depth);
    }

    let mut layers = Vec::<Vec<usize>>::new();
    for (step, depth) in depth_by_step {
        if layers.len() <= depth {
            layers.resize_with(depth + 1, Vec::new);
        }
        layers[depth].push(step);
    }

    Ok(layers)
}

fn step_var(step: &Step) -> Sym {
    match step {
        Step::Let { var_name, .. }
        | Step::FetchHttp { var_name, .. }
        | Step::CallGrpc { var_name, .. }
        | Step::QueryDb { var_name, .. }
        | Step::Pipe { var_name, .. } => *var_name,
    }
}

fn step_kind(step: &Step) -> StepKind {
    match step {
        Step::Let { .. } => StepKind::Let,
        Step::FetchHttp { .. } => StepKind::FetchHttp,
        Step::CallGrpc { .. } => StepKind::CallGrpc,
        Step::QueryDb { .. } => StepKind::QueryDb,
        Step::Pipe { .. } => StepKind::Pipe,
    }
}

fn step_reads(step: &Step) -> BTreeSet<Sym> {
    let mut reads = BTreeSet::new();
    match step {
        Step::Let { value, .. } => collect_expr_reads(value, &mut reads, &BTreeSet::new()),
        Step::FetchHttp { config, .. } => {
            collect_expr_reads(&config.url, &mut reads, &BTreeSet::new());
            if let Some(fallback) = &config.fallback {
                collect_expr_reads(fallback, &mut reads, &BTreeSet::new());
            }
        }
        Step::CallGrpc { config, .. } => {
            collect_expr_reads(&config.payload, &mut reads, &BTreeSet::new());
            if let Some(fallback) = &config.fallback {
                collect_expr_reads(fallback, &mut reads, &BTreeSet::new());
            }
        }
        Step::QueryDb { config, .. } => {
            collect_expr_reads(&config.db_source, &mut reads, &BTreeSet::new());
            for param in &config.params {
                collect_expr_reads(param, &mut reads, &BTreeSet::new());
            }
            if let Some(fallback) = &config.fallback {
                collect_expr_reads(fallback, &mut reads, &BTreeSet::new());
            }
        }
        Step::Pipe {
            source, operations, ..
        } => {
            collect_expr_reads(source, &mut reads, &BTreeSet::new());
            for op in operations {
                match op {
                    PipeOp::Filter { param, condition } => {
                        let mut bound = BTreeSet::new();
                        bound.insert(*param);
                        collect_expr_reads(condition, &mut reads, &bound);
                    }
                    PipeOp::Map { param, layout } => {
                        let mut bound = BTreeSet::new();
                        bound.insert(*param);
                        for expr in layout.values() {
                            collect_expr_reads(expr, &mut reads, &bound);
                        }
                    }
                    PipeOp::Take(_) => {}
                }
            }
        }
    }
    reads
}

fn response_reads(endpoint: &Endpoint) -> BTreeSet<Sym> {
    let mut reads = BTreeSet::new();
    for expr in endpoint.response_body.values() {
        collect_expr_reads(expr, &mut reads, &BTreeSet::new());
    }
    reads
}

fn collect_expr_reads(expr: &Expression, reads: &mut BTreeSet<Sym>, bound: &BTreeSet<Sym>) {
    match expr {
        Expression::Variable(name) => {
            if !bound.contains(name) {
                reads.insert(*name);
            }
        }
        Expression::PropertyAccess(object, _) => collect_expr_reads(object, reads, bound),
        Expression::Call { callee, args } => {
            collect_call_callee_reads(callee, reads, bound);
            for arg in args {
                collect_expr_reads(arg, reads, bound);
            }
        }
        Expression::BinaryOp(left, _, right) => {
            collect_expr_reads(left, reads, bound);
            collect_expr_reads(right, reads, bound);
        }
        Expression::Object(fields) => {
            for expr in fields.values() {
                collect_expr_reads(expr, reads, bound);
            }
        }
        Expression::Array(items) => {
            for expr in items {
                collect_expr_reads(expr, reads, bound);
            }
        }
        Expression::Number(_) | Expression::String(_) | Expression::Boolean(_) => {}
    }
}

fn collect_call_callee_reads(
    callee: &Expression,
    reads: &mut BTreeSet<Sym>,
    bound: &BTreeSet<Sym>,
) {
    match callee {
        Expression::PropertyAccess(object, _) => collect_expr_reads(object, reads, bound),
        Expression::Variable(_) => {}
        _ => collect_expr_reads(callee, reads, bound),
    }
}

fn sym(interner: &Rodeo, name: Sym) -> String {
    interner.resolve(&name).to_string()
}

fn dot_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::{PlanError, build_plan};
    use crate::parser::Parser;
    use lasso::Rodeo;

    #[test]
    fn builds_parallel_layers_for_independent_fetches() {
        let source = include_str!("../examples/main.gate");
        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("main.gate should parse");
        let plan = build_plan(&ast, &parser.interner).expect("main.gate should plan");
        let endpoint = &plan.endpoints[0];

        assert_eq!(endpoint.layers, vec![vec![0, 1], vec![2], vec![3]]);
        assert_eq!(endpoint.steps[0].produces, "user");
        assert_eq!(endpoint.steps[1].produces, "weather");
        assert_eq!(endpoint.steps[2].dependencies, vec![0]);
        assert_eq!(endpoint.steps[3].dependencies, vec![2]);
    }

    #[test]
    fn rejects_undefined_variables() {
        let source = r#"
            gateway "api" { port: 8080 }
            endpoint "GET /x" {
                let x = missing.id;
                respond 200 { "x": x }
            }
        "#;
        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("source should parse");
        let err = build_plan(&ast, &parser.interner).expect_err("missing should fail planning");

        assert!(matches!(
            err,
            PlanError::UndefinedVariable {
                variable,
                used_by,
                ..
            } if variable == "missing" && used_by == "x"
        ));
    }

    #[test]
    fn rejects_duplicate_variables() {
        let source = r#"
            gateway "api" { port: 8080 }
            endpoint "GET /x" {
                let x = 1;
                let x = 2;
                respond 200 { "x": x }
            }
        "#;
        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("source should parse");
        let err = build_plan(&ast, &parser.interner).expect_err("duplicate should fail planning");

        assert!(matches!(
            err,
            PlanError::DuplicateVariable {
                variable,
                first_step: 0,
                second_step: 1,
                ..
            } if variable == "x"
        ));
    }
}

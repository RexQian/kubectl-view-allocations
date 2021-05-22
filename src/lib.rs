pub mod metrics;
pub mod qty;
pub mod tree;

// mod human_format;
use anyhow::{anyhow, Context, Result};
use chrono::prelude::*;
use core::convert::TryFrom;
use itertools::Itertools;
use prettytable::{cell, format, row, Cell, Row, Table};
use qty::Qty;
use std::collections::BTreeMap;
use std::str::FromStr;
use structopt::clap::arg_enum;
use structopt::clap::AppSettings;
use structopt::StructOpt;
use tracing::{instrument, warn};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, ObjectList, Request};

#[derive(Debug, Clone, Default)]
pub struct Location {
    pub node_name: Option<String>,
    pub namespace: Option<String>,
    pub pod_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub kind: String,
    pub quantity: Qty,
    pub location: Location,
    pub qualifier: ResourceQualifier,
}

#[derive(Debug, Clone)]
pub enum ResourceQualifier {
    Limit,
    Requested,
    Allocatable,
    Utilization,
}

#[derive(Debug, Clone, Default)]
pub struct QtyByQualifier {
    pub limit: Option<Qty>,
    pub requested: Option<Qty>,
    pub allocatable: Option<Qty>,
    pub utilization: Option<Qty>,
}

fn add(lhs: Option<Qty>, rhs: &Qty) -> Option<Qty> {
    lhs.map(|l| &l + rhs).or_else(|| Some(rhs.clone()))
}

impl QtyByQualifier {
    pub fn calc_free(&self) -> Option<Qty> {
        let total_used = std::cmp::max(self.limit.as_ref(), self.requested.as_ref());
        self.allocatable
            .as_ref()
            .zip(total_used)
            .map(|(allocatable, total_used)| {
                if allocatable > total_used {
                    allocatable - total_used
                } else {
                    Qty::default()
                }
            })
    }
}

pub fn sum_by_qualifier(rsrcs: &[&Resource]) -> Option<QtyByQualifier> {
    if !rsrcs.is_empty() {
        let kind = rsrcs
            .get(0)
            .expect("group contains at least 1 element")
            .kind
            .clone();

        if rsrcs.iter().all(|i| i.kind == kind) {
            let sum = rsrcs.iter().fold(QtyByQualifier::default(), |mut acc, v| {
                match &v.qualifier {
                    ResourceQualifier::Limit => acc.limit = add(acc.limit, &v.quantity),
                    ResourceQualifier::Requested => acc.requested = add(acc.requested, &v.quantity),
                    ResourceQualifier::Allocatable => {
                        acc.allocatable = add(acc.allocatable, &v.quantity)
                    }
                    ResourceQualifier::Utilization => {
                        acc.utilization = add(acc.utilization, &v.quantity)
                    }
                };
                acc
            });
            Some(sum)
        } else {
            None
        }
    } else {
        None
    }
}

fn make_qualifiers(
    rsrcs: &[Resource],
    group_by: &[GroupBy],
    resource_names: &[String],
) -> Vec<(Vec<String>, Option<QtyByQualifier>)> {
    let group_by_fct = group_by.iter().map(GroupBy::to_fct).collect::<Vec<_>>();
    let mut out = make_group_x_qualifier(
        &(rsrcs
            .iter()
            .filter(|a| accept_resource(&a.kind, resource_names))
            .collect::<Vec<_>>()),
        &[],
        &group_by_fct,
        0,
    );
    out.sort_by_key(|i| i.0.clone());
    out
}

fn make_group_x_qualifier(
    rsrcs: &[&Resource],
    prefix: &[String],
    group_by_fct: &[fn(&Resource) -> Option<String>],
    group_by_depth: usize,
) -> Vec<(Vec<String>, Option<QtyByQualifier>)> {
    // Note: The `&` is significant here, `GroupBy` is iterable
    // only by reference. You can also call `.into_iter()` explicitly.
    let mut out = vec![];
    if let Some(group_by) = group_by_fct.get(group_by_depth) {
        for (key, group) in rsrcs
            .iter()
            .filter_map(|e| group_by(e).map(|k| (k, *e)))
            .into_group_map()
        {
            let mut key_full = prefix.to_vec();
            key_full.push(key);
            let children =
                make_group_x_qualifier(&group, &key_full, group_by_fct, group_by_depth + 1);
            out.push((key_full, sum_by_qualifier(&group)));
            out.extend(children);
        }
    }
    // let kg = &rsrcs.into_iter().group_by(|v| v.kind);
    // kg.into_iter().map(|(key, group)|  ).collect()
    out
}

fn accept_resource(name: &str, resource_filter: &[String]) -> bool {
    resource_filter.is_empty() || resource_filter.iter().any(|x| name.contains(x))
}

#[instrument(skip(client, resources))]
pub async fn collect_from_nodes(client: kube::Client, resources: &mut Vec<Resource>) -> Result<()> {
    let api_nodes: Api<Node> = Api::all(client);
    let nodes = api_nodes
        .list(&ListParams::default())
        .await
        .with_context(|| "Failed to list nodes via k8s api".to_string())?;
    for node in nodes.items {
        let location = Location {
            node_name: node.metadata.name,
            ..Location::default()
        };
        if let Some(als) = node.status.and_then(|v| v.allocatable) {
            // add_resource(resources, &location, ResourceUsage::Allocatable, &als)?
            for (kind, value) in als.iter() {
                let quantity = Qty::from_str(&(value).0).with_context(|| {
                    format!(
                        "Failed to read Qty of location {:?} / {:?} {:?}={:?}",
                        &location,
                        ResourceQualifier::Allocatable,
                        kind,
                        &value
                    )
                })?;
                resources.push(Resource {
                    kind: kind.clone(),
                    qualifier: ResourceQualifier::Allocatable,
                    quantity,
                    location: location.clone(),
                });
            }
        }
    }
    Ok(())
}

/*
The phase of a Pod is a simple, high-level summary of where the Pod is in its lifecycle. The conditions array, the reason and message fields, and the individual container status arrays contain more detail about the pod's status.

There are five possible phase values:
Pending: The pod has been accepted by the Kubernetes system, but one or more of the container images has not been created. This includes time before being scheduled as well as time spent downloading images over the network, which could take a while.
Running: The pod has been bound to a node, and all of the containers have been created. At least one container is still running, or is in the process of starting or restarting.
Succeeded: All containers in the pod have terminated in success, and will not be restarted.
Failed: All containers in the pod have terminated, and at least one container has terminated in failure. The container either exited with non-zero status or was terminated by the system.
Unknown: For some reason the state of the pod could not be obtained, typically due to an error in communicating with the host of the pod.

More info: https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle#pod-phase
*/

pub fn is_scheduled(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|ps| {
            ps.phase.as_ref().and_then(|phase| {
                match &phase[..] {
                    "Succeeded" | "Failed" => Some(false),
                    "Running" => Some(true),
                    "Unknown" => None, // this is the case when a node is down (kubelet is not responding)
                    "Pending" => ps.conditions.as_ref().map(|s| {
                        s.iter()
                            .any(|c| c.type_ == "PodScheduled" && c.status == "True")
                    }),
                    &_ => None, // should not happen
                }
            })
        })
        .unwrap_or(false)
}

fn push_resources(
    resources: &mut Vec<Resource>,
    location: &Location,
    qualifier: ResourceQualifier,
    resource_list: &BTreeMap<String, Qty>,
) -> Result<()> {
    for (key, quantity) in resource_list.iter() {
        resources.push(Resource {
            kind: key.clone(),
            qualifier: qualifier.clone(),
            quantity: quantity.clone(),
            location: location.clone(),
        });
    }
    // add a "pods" resource as well
    resources.push(Resource {
        kind: "pods".to_string(),
        qualifier: qualifier.clone(),
        quantity: Qty::from_str("1")?,
        location: location.clone(),
    });
    Ok(())
}

fn process_resources<F>(
    effective_resources: &mut BTreeMap<String, Qty>,
    resource_list: &BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>,
    op: F,
) -> Result<()>
where
    F: Fn(Qty, Qty) -> Qty,
{
    for (key, value) in resource_list.iter() {
        let quantity = Qty::from_str(&(value).0)?;
        if let Some(current_quantity) = effective_resources.get_mut(key) {
            *current_quantity = op(current_quantity.clone(), quantity).clone();
        } else {
            effective_resources.insert(key.clone(), quantity.clone());
        }
    }
    Ok(())
}

#[instrument(skip(client, resources))]
pub async fn collect_from_pods(
    client: kube::Client,
    resources: &mut Vec<Resource>,
    namespace: &Option<String>,
) -> Result<()> {
    let api_pods: Api<Pod> = if let Some(ns) = namespace {
        Api::namespaced(client, &ns)
    } else {
        Api::all(client)
    };
    let pods = api_pods
        .list(&ListParams::default())
        .await
        .with_context(|| "Failed to list pods via k8s api".to_string())?;
    for pod in pods.items.into_iter().filter(is_scheduled) {
        let spec = pod.spec.as_ref();
        let node_name = spec.and_then(|s| s.node_name.clone());
        let metadata = &pod.metadata;
        let location = Location {
            node_name: node_name.clone(),
            namespace: metadata.namespace.clone(),
            pod_name: metadata.name.clone(),
        };
        // compute the effective resource qualifier
        // see https://kubernetes.io/docs/concepts/workloads/pods/init-containers/#resources
        let mut resource_requests: BTreeMap<String, Qty> = BTreeMap::new();
        let mut resource_limits: BTreeMap<String, Qty> = BTreeMap::new();
        // handle regular containers
        let containers = spec.map(|s| s.containers.clone()).unwrap_or_default();
        for container in containers.into_iter() {
            if let Some(requirements) = container.resources {
                if let Some(r) = requirements.requests {
                    process_resources(&mut resource_requests, &r, std::ops::Add::add)?
                }
                if let Some(l) = requirements.limits {
                    process_resources(&mut resource_limits, &l, std::ops::Add::add)?
                }
            }
        }
        // handle initContainers
        let init_containers = spec
            .and_then(|s| s.init_containers.clone())
            .unwrap_or_default();
        for container in init_containers.into_iter() {
            if let Some(requirements) = container.resources {
                if let Some(r) = requirements.requests {
                    process_resources(&mut resource_requests, &r, std::cmp::max)?
                }
                if let Some(l) = requirements.limits {
                    process_resources(&mut resource_limits, &l, std::cmp::max)?
                }
            }
        }
        // handler overhead (add to both requests and limits)
        if let Some(overhead) = spec.and_then(|s| s.overhead.as_ref()) {
            process_resources(&mut resource_requests, &overhead, std::ops::Add::add)?;
            process_resources(&mut resource_limits, &overhead, std::ops::Add::add)?;
        }
        // push these onto resources
        push_resources(
            resources,
            &location,
            ResourceQualifier::Requested,
            &resource_requests,
        )?;
        push_resources(
            resources,
            &location,
            ResourceQualifier::Limit,
            &resource_limits,
        )?;
    }
    Ok(())
}

pub fn extract_locations(
    resources: &Vec<Resource>,
) -> std::collections::HashMap<(String, String), Location> {
    resources
        .iter()
        .filter_map(|resource| {
            let loc = &resource.location;
            loc.pod_name.as_ref().map(|n| {
                (
                    (loc.namespace.clone().unwrap_or_default(), n.to_owned()),
                    loc.clone(),
                )
            })
        })
        .collect()
}

//TODO need location of pods (aka node because its not part of metrics)
#[instrument(skip(client, resources))]
pub async fn collect_from_metrics(
    client: kube::Client,
    resources: &mut Vec<Resource>,
) -> Result<()> {
    let request = Request::new("/apis/metrics.k8s.io/v1beta1/pods");
    let pod_metrics: ObjectList<metrics::PodMetrics> = client
        .request(request.list(&ListParams::default())?)
        .await
        .with_context(|| {
            "Failed to list podmetrics, maybe Metrics API not available".to_string()
        })?;
    let cpu_kind = "cpu";
    let memory_kind = "memory";
    let locations = extract_locations(resources);
    for pod_metric in pod_metrics.items {
        let metadata = &pod_metric.metadata;
        let key = (
            metadata.namespace.clone().unwrap_or_default(),
            metadata.name.clone().unwrap_or_default(),
        );
        let location = locations.get(&key).cloned().unwrap_or_else(|| Location {
            // node_name: node_name.clone(),
            namespace: metadata.namespace.clone(),
            pod_name: metadata.name.clone(),
            ..Location::default()
        });
        let mut cpu_utilization = Qty::default();
        let mut memory_utilization = Qty::default();
        for container in pod_metric.containers.into_iter() {
            cpu_utilization += &Qty::from_str(&container.usage.cpu)
                .with_context(|| {
                    format!(
                        "Failed to read Qty of location {:?} / {:?} {:?}={:?}",
                        &location,
                        ResourceQualifier::Utilization,
                        cpu_kind,
                        &container.usage.cpu
                    )
                })?
                .max(Qty::lowest_positive());
            memory_utilization += &Qty::from_str(&container.usage.memory)
                .with_context(|| {
                    format!(
                        "Failed to read Qty of location {:?} / {:?} {:?}={:?}",
                        &location,
                        ResourceQualifier::Utilization,
                        memory_kind,
                        &container.usage.memory
                    )
                })?
                .max(Qty::lowest_positive());
        }
        resources.push(Resource {
            kind: cpu_kind.to_string(),
            qualifier: ResourceQualifier::Utilization,
            quantity: cpu_utilization,
            location: location.clone(),
        });
        resources.push(Resource {
            kind: memory_kind.to_string(),
            qualifier: ResourceQualifier::Utilization,
            quantity: memory_utilization,
            location: location.clone(),
        });
    }
    Ok(())
}

arg_enum! {
    #[derive(Debug, Eq, PartialEq)]
    #[allow(non_camel_case_types)]
    pub enum GroupBy {
        resource,
        node,
        pod,
        namespace,
    }
}

impl GroupBy {
    pub fn to_fct(&self) -> fn(&Resource) -> Option<String> {
        match self {
            Self::resource => Self::extract_kind,
            Self::node => Self::extract_node_name,
            Self::pod => Self::extract_pod_name,
            Self::namespace => Self::extract_namespace,
        }
    }

    fn extract_kind(e: &Resource) -> Option<String> {
        Some(e.kind.clone())
    }

    fn extract_node_name(e: &Resource) -> Option<String> {
        e.location.node_name.clone()
    }

    fn extract_pod_name(e: &Resource) -> Option<String> {
        // We do not need to display "pods" resource types when grouping by pods
        if e.kind == "pods" {
            return None;
        }
        e.location.pod_name.clone()
    }

    fn extract_namespace(e: &Resource) -> Option<String> {
        e.location.namespace.clone()
    }
}

arg_enum! {
    #[derive(Debug, Eq, PartialEq)]
    #[allow(non_camel_case_types)]
    pub enum Output {
        table,
        csv,
    }
}

#[derive(StructOpt, Debug)]
#[structopt(
    global_settings(&[AppSettings::ColoredHelp, AppSettings::VersionlessSubcommands]),
    author = env!("CARGO_PKG_HOMEPAGE"), about
)]
pub struct CliOpts {
    /// The name of the kubeconfig context to use
    #[structopt(long)]
    pub context: Option<String>,

    /// Show only pods from this namespace
    #[structopt(short, long)]
    pub namespace: Option<String>,

    /// Force to retrieve utilization (for cpu and memory), require to have metrics-server https://github.com/kubernetes-sigs/metrics-server
    #[structopt(short = "u", long)]
    pub utilization: bool,

    /// Show lines with zero requested and zero limit and zero allocatable
    #[structopt(short = "z", long)]
    pub show_zero: bool,

    /// Filter resources shown by name(s), by default all resources are listed
    #[structopt(short, long)]
    pub resource_name: Vec<String>,

    /// Group information hierarchically (default: -g resource -g node -g pod)
    #[structopt(short, long, possible_values = &GroupBy::variants(), case_insensitive = true)]
    pub group_by: Vec<GroupBy>,

    /// Output format
    #[structopt(short, long, possible_values = &Output::variants(), case_insensitive = true, default_value = "table")]
    pub output: Output,
}

pub async fn refresh_kube_config(cli_opts: &CliOpts) -> Result<()> {
    //HACK force refresh token by calling "kubectl cluster-info before loading configuration"
    use std::process::Command;
    let mut cmd = Command::new("kubectl");
    cmd.arg("cluster-info");
    if let Some(ref context) = cli_opts.context {
        cmd.arg("--context").arg(context);
    }
    let output = cmd
        .output()
        .with_context(|| "failed to executed 'kubectl cluster-info'")?;
    if !output.status.success() {
        return Err(anyhow!("`kubectl cluster-info` failed with: {:?}", &output));
    }
    Ok(())
}

pub async fn new_client(cli_opts: &CliOpts) -> Result<kube::Client> {
    refresh_kube_config(cli_opts)
        .await
        .with_context(|| "failed to refresh kubectl config".to_string())?;
    let client_config = match cli_opts.context {
        Some(ref context) => {
            kube::Config::from_kubeconfig(&kube::config::KubeConfigOptions {
                context: Some(context.clone()),
                ..Default::default()
            })
            .await?
        }
        None => kube::Config::infer().await?,
    };
    kube::Client::try_from(client_config)
        .with_context(|| "failed to create the kube client".to_string())
}

#[instrument]
pub async fn do_main(cli_opts: &CliOpts) -> Result<()> {
    let client = new_client(cli_opts).await?;
    let mut resources: Vec<Resource> = vec![];
    collect_from_nodes(client.clone(), &mut resources)
        .await
        .with_context(|| "failed to collect info from nodes".to_string())?;
    collect_from_pods(client.clone(), &mut resources, &cli_opts.namespace)
        .await
        .with_context(|| "failed to collect info from pods".to_string())?;
    let show_utilization = match collect_from_metrics(client.clone(), &mut resources).await {
        Ok(_) => true,
        Err(err) => {
            if cli_opts.utilization {
                warn!("{:?}", err);
            }
            false
        }
    };

    let res = make_qualifiers(&resources, &cli_opts.group_by, &cli_opts.resource_name);
    match &cli_opts.output {
        Output::table => display_with_prettytable(&res, !&cli_opts.show_zero, show_utilization),
        Output::csv => display_as_csv(&res, &cli_opts.group_by, show_utilization),
    }
    Ok(())
}

pub fn display_as_csv(
    data: &[(Vec<String>, Option<QtyByQualifier>)],
    group_by: &[GroupBy],
    show_utilization: bool,
) {
    // print header
    println!(
        "Date,Kind,{}{},Requested,%Requested,Limit,%Limit,Allocatable,Free",
        group_by.iter().map(|x| x.to_string()).join(","),
        if show_utilization {
            ",Utilization,%Utilization"
        } else {
            ""
        }
    );

    // print data
    let empty = "".to_string();
    let datetime = Utc::now().to_rfc3339();
    for (k, oqtys) in data {
        if let Some(qtys) = oqtys {
            let mut row = vec![];
            row.push(datetime.clone());
            row.push(
                group_by
                    .get(k.len() - 1)
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| empty.clone()),
            );
            for i in 0..group_by.len() {
                row.push(k.get(i).cloned().unwrap_or_else(|| empty.clone()));
            }

            if show_utilization {
                add_cells_for_cvs(&mut row, &qtys.utilization, &qtys.allocatable);
            }
            add_cells_for_cvs(&mut row, &qtys.requested, &qtys.allocatable);
            add_cells_for_cvs(&mut row, &qtys.limit, &qtys.allocatable);

            row.push(
                qtys.allocatable
                    .as_ref()
                    .map(|qty| format!("{:.2}", f64::from(qty)))
                    .unwrap_or_else(|| empty.clone()),
            );
            row.push(
                qtys.calc_free()
                    .as_ref()
                    .map(|qty| format!("{:.2}", f64::from(qty)))
                    .unwrap_or_else(|| empty.clone()),
            );
            println!("{}", &row.join(","));
        }
    }
}

fn add_cells_for_cvs(row: &mut Vec<String>, oqty: &Option<Qty>, o100: &Option<Qty>) {
    match oqty {
        None => {
            row.push("".to_string());
            row.push("".to_string());
        }
        Some(ref qty) => {
            row.push(format!("{:.2}", f64::from(qty)));
            row.push(match o100 {
                None => "".to_string(),
                Some(q100) => format!("{:.0}%", qty.calc_percentage(&q100)),
            });
        }
    };
}

pub fn display_with_prettytable(
    data: &[(Vec<String>, Option<QtyByQualifier>)],
    filter_full_zero: bool,
    show_utilization: bool,
) {
    // Create the table
    let mut table = Table::new();
    let format = format::FormatBuilder::new()
        // .column_separator('|')
        // .borders('|')
        // .separators(&[format::LinePosition::Top,
        //               format::LinePosition::Bottom],
        //             format::LineSeparator::new('-', '+', '+', '+'))
        .separators(&[], format::LineSeparator::new('-', '+', '+', '+'))
        .padding(1, 1)
        .build();
    table.set_format(format);
    let mut row_titles = row![bl->"Resource", br->"Utilization", br->"Requested", br->"Limit",  br->"Allocatable", br->"Free"];
    if !show_utilization {
        row_titles.remove_cell(1);
    }
    table.set_titles(row_titles);
    let data2 = data
        .iter()
        .filter(|d| {
            !filter_full_zero
                || !d
                    .1
                    .as_ref()
                    .map(|x| {
                        x.utilization.is_none()
                            && is_empty(&x.requested)
                            && is_empty(&x.limit)
                            && is_empty(&x.allocatable)
                    })
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    let prefixes = tree::provide_prefix(&data2, |parent, item| parent.0.len() + 1 == item.0.len());

    for ((k, oqtys), prefix) in data2.iter().zip(prefixes.iter()) {
        let column0 = format!(
            "{} {}",
            prefix,
            k.last().map(|x| x.as_str()).unwrap_or("???")
        );
        if let Some(qtys) = oqtys {
            let style = if qtys.requested > qtys.limit || qtys.utilization > qtys.limit {
                "rFy"
            } else if is_empty(&qtys.requested) || is_empty(&qtys.limit) {
                "rFy"
            } else {
                "rFg"
            };
            let mut row = Row::new(vec![
                Cell::new(&column0),
                make_cell_for_prettytable(&qtys.utilization, &qtys.allocatable).style_spec(style),
                make_cell_for_prettytable(&qtys.requested, &qtys.allocatable).style_spec(style),
                make_cell_for_prettytable(&qtys.limit, &qtys.allocatable).style_spec(style),
                make_cell_for_prettytable(&qtys.allocatable, &None).style_spec(style),
                make_cell_for_prettytable(&qtys.calc_free(), &None).style_spec(style),
            ]);
            if !show_utilization {
                row.remove_cell(1);
            }
            table.add_row(row);
        }
    }

    // Print the table to stdout
    table.printstd();
}

fn is_empty(oqty: &Option<Qty>) -> bool {
    match oqty {
        Some(qty) => qty.is_zero(),
        None => true,
    }
}

fn make_cell_for_prettytable(oqty: &Option<Qty>, o100: &Option<Qty>) -> Cell {
    let txt = match oqty {
        None => "__".to_string(),
        Some(ref qty) => match o100 {
            None => format!("{}", qty.adjust_scale()),
            Some(q100) => format!("({:.0}%) {}", qty.calc_percentage(q100), qty.adjust_scale()),
        },
    };
    Cell::new(&txt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accept_resource() {
        assert_eq!(accept_resource("cpu", &vec![]), true);
        assert_eq!(accept_resource("cpu", &vec!["c".to_string()]), true);
        assert_eq!(accept_resource("cpu", &vec!["cpu".to_string()]), true);
        assert_eq!(accept_resource("cpu", &vec!["cpu3".to_string()]), false);
        assert_eq!(accept_resource("gpu", &vec!["gpu".to_string()]), true);
        assert_eq!(
            accept_resource("nvidia.com/gpu", &vec!["gpu".to_string()]),
            true
        );
    }
}

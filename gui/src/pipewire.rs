//! Routing another application's audio into the capture, through PipeWire.
//!
//! Audio from a WebSDR in a browser is the common way to listen to a band you
//! have no antenna for, and on a PipeWire desktop getting it into a decoder is
//! the fiddly part. The route is a link between two graph nodes: the browser's
//! output stream and *this* program's capture stream. Neither exists until both
//! programs are running and playing, so it cannot be set up in advance, and the
//! usual tools for making it — `qpwgraph`, `pavucontrol`'s recording tab — mean
//! leaving the app to go and patch it somewhere else.
//!
//! Worse, the route dies with the stream. Reopening the capture, which is what
//! changing the input device does, destroys the node the link was attached to
//! and drops the operator back at the patch bay. So this deliberately does
//! *not* touch the capture: it rewires the graph around a stream that stays
//! open, which is the only way the route survives being made.
//!
//! Everything here shells out to `pw-dump` and `pw-link`. They ship with
//! PipeWire itself, they are the documented interface to the graph, and the
//! alternative — linking libpipewire and running a main loop — is a great deal
//! of machinery for four links.

use std::process::Command;

use serde_json::Value;

/// An application currently producing audio, as something to listen to.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    /// PipeWire node id of the application's output stream.
    pub node: u32,
    /// The application, e.g. `Firefox`.
    pub app: String,
    /// What it is playing, e.g. `Northern Utah WebSDR #2`. Often the browser
    /// tab title, which is exactly what identifies the right one when a browser
    /// has several making noise.
    pub title: String,
}

impl Source {
    /// One line for a menu: the application, and what it is playing if that
    /// says anything the application name does not.
    pub fn label(&self) -> String {
        match self.title.trim() {
            "" => self.app.clone(),
            t if t.eq_ignore_ascii_case(&self.app) => self.app.clone(),
            t => format!("{} — {t}", self.app),
        }
    }
}

/// What can be routed, and what is routed now.
#[derive(Clone, Debug, Default)]
pub struct Routing {
    /// Whether routing can be offered at all: PipeWire's tools are here, and
    /// this program's capture is a node in its graph rather than a direct ALSA
    /// device.
    pub available: bool,
    /// Applications currently producing audio, newest stream first — a stream
    /// that has only just started is the one the operator has just gone and
    /// started, so it is the likeliest thing they mean.
    pub sources: Vec<Source>,
    /// The one currently feeding this program's capture, if any.
    pub current: Option<Source>,
}

/// Look at the graph once and answer everything a menu needs to draw itself.
///
/// One pass rather than a call per question, because a menu redraws every frame
/// and each of these is a subprocess and a JSON parse. The caller is expected to
/// hold the answer and refresh it at human speed.
pub fn survey() -> Routing {
    let Ok(g) = dump() else { return Routing::default() };
    let sources = g.sources();
    let current = g.our_node().and_then(|ours| {
        let feeding: Vec<u32> = g.links_into(ours).iter().map(|l| l.out_node).collect();
        sources.iter().find(|s| feeding.contains(&s.node)).cloned()
    });
    Routing { available: g.our_node().is_some() && which("pw-link"), sources, current }
}

fn which(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok_and(|o| o.status.success())
}

/// Route `source`'s audio into this program's capture, in place of whatever was
/// feeding it.
///
/// The capture stream is left alone: this is graph surgery around a stream that
/// keeps running, so the route outlives the making of it. Returns what the
/// route now is, for the log.
pub fn route_from(source: &Source) -> Result<String, String> {
    let g = dump()?;
    let plan = g.plan(source.node).map_err(|e| format!("{}: {e}", source.app))?;

    // The old links go first. Left in place PipeWire sums them with the new
    // ones, and a microphone mixed into a WebSDR is not a band, it is a room.
    for id in &plan.remove {
        run("pw-link", &["-d".into(), id.to_string()])?;
    }
    for (out, inp) in &plan.link {
        run("pw-link", &[out.to_string(), inp.to_string()])?;
    }
    Ok(format!("{} -> this capture, {} channel(s)", source.label(), plan.link.len()))
}

/// What routing one application in comes down to: links to remove, and pairs of
/// ports to join.
///
/// Separated from the doing of it so the decision can be tested against a real
/// graph, which is the half with judgement in it — the other half is running
/// `pw-link` twice.
#[derive(Debug, PartialEq)]
struct Plan {
    remove: Vec<u32>,
    link: Vec<(u32, u32)>,
}

/// The channel part of a port name: `output_FL` and `input_FL` are both `FL`.
fn channel(port: &str) -> &str {
    port.rsplit('_').next().unwrap_or(port)
}

fn run(cmd: &str, args: &[String]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("could not run {cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---- the graph, as much of it as any of this needs ----

struct Node {
    id: u32,
    serial: u64,
    class: String,
    client: Option<u32>,
    app: String,
    title: String,
}

struct Port {
    id: u32,
    node: u32,
    dir: String,
    name: String,
}

struct Link {
    id: u32,
    out_node: u32,
    in_node: u32,
}

struct Graph {
    nodes: Vec<Node>,
    ports: Vec<Port>,
    links: Vec<Link>,
    /// PipeWire client ids belonging to this process.
    ours: Vec<u32>,
}

fn dump() -> Result<Graph, String> {
    let text = run("pw-dump", &[])?;
    let objs: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("pw-dump is not JSON: {e}"))?;
    Ok(parse(&objs, std::process::id()))
}

/// Pick the graph apart. Separate from [`dump`] so it can be tested against a
/// real `pw-dump` capture rather than against a live daemon.
fn parse(objs: &[Value], pid: u32) -> Graph {
    let (mut nodes, mut ports, mut links, mut ours) = (Vec::new(), Vec::new(), Vec::new(), vec![]);
    for o in objs {
        let id = o["id"].as_u64().unwrap_or_default() as u32;
        let ty = o["type"].as_str().unwrap_or_default();
        let p = &o["info"]["props"];
        let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        match ty.rsplit(':').next().unwrap_or_default() {
            // The ALSA plugin's node carries no pid of its own, but its client
            // does, and that is what makes "our capture" exact rather than a
            // guess at a name.
            "Client" if p["application.process.id"].as_u64() == Some(pid as u64) => ours.push(id),
            "Node" => nodes.push(Node {
                id,
                serial: p["object.serial"].as_u64().unwrap_or_default(),
                class: s("media.class"),
                client: p["client.id"].as_u64().map(|c| c as u32),
                app: s("application.name"),
                title: s("media.name"),
            }),
            "Port" => ports.push(Port {
                id,
                node: p["node.id"].as_u64().unwrap_or_default() as u32,
                dir: s("port.direction"),
                name: s("port.name"),
            }),
            "Link" => links.push(Link {
                id,
                out_node: p["link.output.node"].as_u64().unwrap_or_default() as u32,
                in_node: p["link.input.node"].as_u64().unwrap_or_default() as u32,
            }),
            _ => {}
        }
    }
    Graph { nodes, ports, links, ours }
}

impl Graph {
    /// This program's capture node.
    ///
    /// Found through the client that owns it, so a second copy of the app
    /// running alongside is not mistaken for this one — they would otherwise be
    /// indistinguishable, since the ALSA plugin names every capture node after
    /// the program.
    fn our_node(&self) -> Option<u32> {
        self.nodes
            .iter()
            .filter(|n| {
                n.class == "Stream/Input/Audio" && n.client.is_some_and(|c| self.ours.contains(&c))
            })
            .max_by_key(|n| n.serial)
            .map(|n| n.id)
    }

    fn sources(&self) -> Vec<Source> {
        let mut out: Vec<(u64, Source)> = self
            .nodes
            .iter()
            .filter(|n| n.class == "Stream/Output/Audio")
            .map(|n| {
                let app = if n.app.is_empty() { format!("node {}", n.id) } else { n.app.clone() };
                (n.serial, Source { node: n.id, app, title: n.title.clone() })
            })
            .collect();
        out.sort_by_key(|(serial, _)| std::cmp::Reverse(*serial));
        out.into_iter().map(|(_, s)| s).collect()
    }

    fn links_into(&self, node: u32) -> Vec<&Link> {
        self.links.iter().filter(|l| l.in_node == node).collect()
    }

    /// How to make `source` the thing feeding this program's capture.
    ///
    /// Channels are paired by name where both ends name them, because a capture
    /// node does not always have the same channel count as the stream being
    /// routed into it and lining them up by position would put the left channel
    /// somewhere surprising. A genuinely mono source is the exception: it feeds
    /// every input, or half the capture is silence.
    fn plan(&self, source: u32) -> Result<Plan, String> {
        let ours = self
            .our_node()
            .ok_or("this capture is not a PipeWire stream, so there is nothing to route into")?;
        let ins = self.ports_of(ours, "in");
        let outs = self.ports_of(source, "out");
        if ins.is_empty() || outs.is_empty() {
            return Err("no audio ports to route from".into());
        }

        let mut link: Vec<(u32, u32)> = if outs.len() == 1 {
            ins.iter().map(|i| (outs[0].id, i.id)).collect()
        } else {
            ins.iter()
                .filter_map(|i| {
                    outs.iter().find(|o| channel(&o.name) == channel(&i.name)).map(|o| (o.id, i.id))
                })
                .collect()
        };
        // Nothing agreed on a name: fall back to position, which is all that is
        // left to go on and is right more often than it is wrong.
        if link.is_empty() {
            link = outs.iter().zip(ins.iter()).map(|(o, i)| (o.id, i.id)).collect();
        }
        Ok(Plan { remove: self.links_into(ours).iter().map(|l| l.id).collect(), link })
    }

    /// A node's ports in one direction, in channel order.
    fn ports_of(&self, node: u32, dir: &str) -> Vec<&Port> {
        let mut v: Vec<&Port> =
            self.ports.iter().filter(|p| p.node == node && p.dir == dir).collect();
        v.sort_by_key(|p| (channel_order(&p.name), p.id));
        v
    }
}

/// Where a channel sits in the conventional order, so ports come out
/// front-left first however the daemon happened to list them.
fn channel_order(port: &str) -> usize {
    const ORDER: [&str; 8] = ["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"];
    ORDER.iter().position(|c| *c == channel(port)).unwrap_or(ORDER.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `pw-dump`, trimmed to the objects this reads: a browser playing a
    /// WebSDR, this program capturing, and the microphone it came up on.
    const DUMP: &str = r#"[
      {"id":97,"type":"PipeWire:Interface:Client","info":{"props":{
        "application.name":"PipeWire ALSA [ragchew]","application.process.id":133476}}},
      {"id":101,"type":"PipeWire:Interface:Client","info":{"props":{
        "application.name":"Firefox","application.process.id":13728}}},
      {"id":115,"type":"PipeWire:Interface:Node","info":{"props":{
        "media.class":"Stream/Output/Audio","client.id":101,"object.serial":402,
        "application.name":"Firefox","media.name":"Northern Utah WebSDR #2 - 30-6 MOmni"}}},
      {"id":90,"type":"PipeWire:Interface:Node","info":{"props":{
        "media.class":"Stream/Input/Audio","client.id":97,"object.serial":381,
        "application.name":"PipeWire ALSA [ragchew]","media.name":"ALSA Capture"}}},
      {"id":69,"type":"PipeWire:Interface:Node","info":{"props":{
        "media.class":"Audio/Source","object.serial":42,
        "node.name":"alsa_input.pci-0000_00_1f.3.HiFi__Mic1__source"}}},
      {"id":118,"type":"PipeWire:Interface:Port","info":{"props":{
        "node.id":115,"port.direction":"out","port.name":"output_FL"}}},
      {"id":91,"type":"PipeWire:Interface:Port","info":{"props":{
        "node.id":115,"port.direction":"out","port.name":"output_FR"}}},
      {"id":113,"type":"PipeWire:Interface:Port","info":{"props":{
        "node.id":90,"port.direction":"in","port.name":"input_RR"}}},
      {"id":107,"type":"PipeWire:Interface:Port","info":{"props":{
        "node.id":90,"port.direction":"in","port.name":"input_FL"}}},
      {"id":117,"type":"PipeWire:Interface:Port","info":{"props":{
        "node.id":90,"port.direction":"in","port.name":"input_FR"}}},
      {"id":92,"type":"PipeWire:Interface:Port","info":{"props":{
        "node.id":90,"port.direction":"out","port.name":"monitor_FL"}}},
      {"id":96,"type":"PipeWire:Interface:Link","info":{"props":{
        "link.output.node":69,"link.output.port":83,"link.input.node":90,"link.input.port":107}}},
      {"id":89,"type":"PipeWire:Interface:Link","info":{"props":{
        "link.output.node":69,"link.output.port":84,"link.input.node":90,"link.input.port":117}}}
    ]"#;

    fn graph() -> Graph {
        parse(&serde_json::from_str::<Vec<Value>>(DUMP).unwrap(), 133476)
    }

    /// The capture to route into is found through the client that owns it, not
    /// by its name — every copy of this program produces a node called
    /// `alsa_capture.ragchew`, and rewiring another instance's audio would be a
    /// baffling thing to do to somebody.
    #[test]
    fn our_own_capture_is_found_by_process_not_by_name() {
        assert_eq!(graph().our_node(), Some(90));

        // The same graph, seen by a different process, holds no capture of ours.
        let other = parse(&serde_json::from_str::<Vec<Value>>(DUMP).unwrap(), 999);
        assert_eq!(other.our_node(), None, "claimed another process's capture");
    }

    /// What the menu offers is applications playing audio — not sinks, not
    /// sources, and not this program's own capture.
    #[test]
    fn the_sources_offered_are_applications_playing_audio() {
        let got = graph().sources();
        assert_eq!(got.len(), 1, "offered {got:?}");
        assert_eq!(got[0].node, 115);
        assert_eq!(got[0].label(), "Firefox — Northern Utah WebSDR #2 - 30-6 MOmni");
    }

    /// A browser with one tab per band is the case that needs the title: two
    /// entries both called Firefox are no help at all.
    #[test]
    fn an_application_with_nothing_to_add_is_named_once() {
        let s = |app: &str, title: &str| Source {
            node: 1,
            app: app.to_string(),
            title: title.to_string(),
        };
        assert_eq!(s("Firefox", "").label(), "Firefox");
        assert_eq!(s("Firefox", "firefox").label(), "Firefox");
        assert_eq!(s("Firefox", "40m WebSDR").label(), "Firefox — 40m WebSDR");
    }

    /// Ports come out front-left first whatever order the daemon listed them,
    /// so the left channel is not silently wired to the right.
    #[test]
    fn ports_are_matched_up_by_channel() {
        let g = graph();
        let ins: Vec<&str> = g.ports_of(90, "in").iter().map(|p| p.name.as_str()).collect();
        assert_eq!(ins, ["input_FL", "input_FR", "input_RR"], "capture ports out of order");
        let outs: Vec<&str> = g.ports_of(115, "out").iter().map(|p| p.name.as_str()).collect();
        assert_eq!(outs, ["output_FL", "output_FR"]);
        assert_eq!(channel("output_FL"), channel("input_FL"));
        assert_ne!(channel("output_FL"), channel("input_FR"));
    }

    /// Whatever was feeding the capture has to be found, because it has to be
    /// taken away: left in place, PipeWire sums it with the new route and the
    /// microphone plays over the band.
    #[test]
    fn the_links_to_be_replaced_are_the_ones_into_our_capture() {
        let g = graph();
        let ids: Vec<u32> = g.links_into(90).iter().map(|l| l.id).collect();
        assert_eq!(ids, [96, 89]);
        assert!(g.links_into(115).is_empty(), "found links into an output stream");
    }

    /// The whole of routing a browser in, as port ids.
    ///
    /// These are the exact `pw-link` calls that were run by hand against the
    /// live daemon this fixture was taken from — drop the two microphone links,
    /// join Firefox's stereo pair to the capture's — so the test pins the
    /// behaviour to something that was watched working rather than to what the
    /// code happens to do.
    #[test]
    fn routing_a_browser_in_is_two_links_out_and_two_in() {
        let want = Plan { remove: vec![96, 89], link: vec![(118, 107), (91, 117)] };
        assert_eq!(graph().plan(115).unwrap(), want);
    }

    /// The capture here has a rear-right input that the browser has nothing to
    /// put in. Leaving it unconnected is the point: the front-left channel
    /// duplicated into it would weight the mono downmix towards one side of a
    /// stereo signal, quietly, for ever.
    #[test]
    fn a_channel_the_source_does_not_have_is_left_alone() {
        let linked: Vec<u32> = graph().plan(115).unwrap().link.iter().map(|(_, i)| *i).collect();
        assert!(!linked.contains(&113), "wired something into the rear-right input");
    }

    /// A mono application is the exception, and has to feed both sides or half
    /// the capture is silence.
    #[test]
    fn a_mono_source_feeds_every_input() {
        let mut objs: Vec<Value> = serde_json::from_str(DUMP).unwrap();
        // Firefox with one port instead of two.
        objs.retain(|o| o["id"].as_u64() != Some(91));
        objs.iter_mut().for_each(|o| {
            if o["id"].as_u64() == Some(118) {
                o["info"]["props"]["port.name"] = Value::from("output_MONO");
            }
        });
        let g = parse(&objs, 133476);
        let plan = g.plan(115).unwrap();
        assert_eq!(plan.link, vec![(118, 107), (118, 117), (118, 113)]);
    }
}

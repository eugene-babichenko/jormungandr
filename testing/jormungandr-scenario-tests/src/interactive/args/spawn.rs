use super::UserInteractionController;
use crate::{style, test::Result};
use jormungandr_testing_utils::testing::{
    network_builder::{LeadershipMode, SpawnParams},
    node::download_last_n_releases,
};
use jortestkit::console::InteractiveCommandError;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
pub enum Spawn {
    Passive(SpawnPassiveNode),
    Leader(SpawnLeaderNode),
}

impl Spawn {
    pub fn exec(&self, controller: &mut UserInteractionController) -> Result<()> {
        match self {
            Spawn::Passive(spawn_passive) => spawn_passive.exec(controller),
            Spawn::Leader(spawn_leader) => spawn_leader.exec(controller),
        }
    }
}

#[derive(StructOpt, Debug)]
pub struct SpawnPassiveNode {
    #[structopt(short = "l", long = "legacy")]
    pub legacy: Option<String>,
    #[structopt(short = "w", long = "wait")]
    pub wait: bool,
    #[structopt(short = "a", long = "alias")]
    pub alias: String,
}

impl SpawnPassiveNode {
    pub fn exec(&self, mut controller: &mut UserInteractionController) -> Result<()> {
        spawn_node(
            &mut controller,
            LeadershipMode::Passive,
            &self.alias,
            self.legacy.clone(),
            self.wait,
        )
    }
}

#[derive(StructOpt, Debug)]
pub struct SpawnLeaderNode {
    #[structopt(short = "s", long = "storage")]
    pub storage: bool,
    #[structopt(short = "l", long = "legacy")]
    pub legacy: Option<String>,
    #[structopt(short = "w", long = "wait")]
    pub wait: bool,
    #[structopt(short = "a", long = "alias")]
    pub alias: String,
}

fn spawn_node(
    controller: &mut UserInteractionController,
    leadership_mode: LeadershipMode,
    alias: &str,
    legacy: Option<String>,
    wait: bool,
) -> Result<()> {
    let mut spawn_params = SpawnParams::new(alias);
    spawn_params.leadership_mode(leadership_mode);

    if let Some(version) = legacy {
        let releases = download_last_n_releases(5);
        let legacy_release = releases
            .iter()
            .find(|x| x.version().eq_ignore_ascii_case(&version))
            .ok_or(InteractiveCommandError::UserError(version.to_string()))?;

        let node = controller.controller_mut().spawn_legacy_node(
            &mut spawn_params,
            &legacy_release.version().parse().unwrap(),
        )?;
        println!(
            "{}",
            style::info.apply_to(format!("node '{}' spawned", alias))
        );

        if wait {
            println!(
                "{}",
                style::info.apply_to("waiting for bootstap...".to_string())
            );
            node.wait_for_bootstrap()?;
            println!(
                "{}",
                style::info.apply_to("node bootstrapped successfully.".to_string())
            );
        }

        controller.legacy_nodes_mut().push(node);
        return Ok(());
    }

    let node = controller
        .controller_mut()
        .spawn_node_custom(&mut spawn_params)?;
    println!(
        "{}",
        style::info.apply_to(format!("node '{}' spawned", alias))
    );

    if wait {
        println!(
            "{}",
            style::info.apply_to("waiting for bootstap...".to_string())
        );
        node.wait_for_bootstrap()?;
        println!(
            "{}",
            style::info.apply_to("node bootstrapped successfully.".to_string())
        );
    }

    controller.nodes_mut().push(node);
    Ok(())
}

impl SpawnLeaderNode {
    pub fn exec(&self, mut controller: &mut UserInteractionController) -> Result<()> {
        spawn_node(
            &mut controller,
            LeadershipMode::Leader,
            &self.alias,
            self.legacy.clone(),
            self.wait,
        )
    }
}

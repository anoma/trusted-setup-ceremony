use phase1_coordinator::{
    environment::{Development, Environment, Parameters, Production},
    Coordinator,
};

use std::time::Duration;
use tokio::{task, time::sleep};
use tracing::*;

#[inline]
async fn coordinator(environment: &Environment) -> anyhow::Result<Coordinator> {
    Ok(Coordinator::new(environment.clone())?)
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    // Set the environment.
    // let environment: Environment = Development::from(Parameters::TestCustom(8, 12, 256)).into();
    let environment: Environment = Production::from(Parameters::AleoInner).into();

    // Instantiate the coordinator.
    let coordinator = coordinator(&environment).await?;

    // Initialize the coordinator.
    let operator = coordinator.clone();
    let ceremony = task::spawn(async move {
        // Initialize the coordinator.
        operator.initialize().unwrap();

        // Initialize the coordinator loop.
        loop {
            // Run the update operation.
            if let Err(error) = operator.update() {
                error!("{}", error);
            }

            // Sleep for 10 seconds in between iterations.
            sleep(Duration::from_secs(10)).await;
        }
    });

    // Initialize the shutdown procedure.
    let handler = coordinator.clone();
    {
        debug!("Initializing the shutdown handler");
        handler.shutdown_listener()?;
    }

    ceremony.await.expect("The ceremony handle has panicked");

    Ok(())
}

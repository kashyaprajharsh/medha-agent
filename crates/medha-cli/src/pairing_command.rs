//! `medha pairing`: the owner, at this computer, decides who may message the agent.

use anyhow::{Result, bail};
use gateway::pairing::Pairing;

const USAGE: &str = "usage: medha pairing list\n       medha pairing approve <platform> <code>\n       medha pairing revoke <platform> <user>\n       medha pairing clear-pending";

pub(crate) fn run(args: &[String]) -> Result<()> {
    let pairing = Pairing::open()?;
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["list"] => list(&pairing),
        ["approve", platform, code @ ..] if !code.is_empty() => {
            match pairing.approve(platform, &code.join(""))? {
                Ok(person) => {
                    println!(
                        "{} ({}) on {} can now message Medha.",
                        shown(&person.name, &person.user),
                        person.user,
                        person.platform
                    );
                    Ok(())
                }
                Err(why) => bail!("{why}"),
            }
        }
        ["revoke", platform, user] => {
            if pairing.revoke(platform, user)? {
                println!("{user} on {platform} can no longer message Medha.");
            } else {
                println!("{user} on {platform} was not paired.");
            }
            Ok(())
        }
        ["clear-pending"] => {
            println!("Cleared {} waiting code(s).", pairing.clear_waiting()?);
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn list(pairing: &Pairing) -> Result<()> {
    let waiting = pairing.waiting()?;
    let approved = pairing.approved()?;
    if waiting.is_empty() && approved.is_empty() {
        println!("Nobody is paired and no code is waiting. Message your bot to get a code.");
        return Ok(());
    }
    if !waiting.is_empty() {
        println!("Waiting for approval (the code is in the message they received):");
        for person in waiting {
            println!(
                "  {:<10} {} ({}), {} min ago",
                person.platform,
                shown(&person.name, &person.user),
                person.user,
                person.minutes
            );
        }
    }
    if !approved.is_empty() {
        println!("Can message Medha:");
        for person in approved {
            println!(
                "  {:<10} {} ({})",
                person.platform,
                shown(&person.name, &person.user),
                person.user
            );
        }
    }
    Ok(())
}

fn shown<'a>(name: &'a str, user: &'a str) -> &'a str {
    if name.trim().is_empty() { user } else { name }
}

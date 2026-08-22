mod engine;
mod plan;
#[cfg(test)]
mod reference;

pub(crate) use engine::{MatchEngine, PostingBlock};
pub(crate) use plan::{MatchPlan, MatchScope};
#[cfg(test)]
pub(crate) use reference::match_subscription;

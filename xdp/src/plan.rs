
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxGroup {
    pub name: &'static str,
    pub ports: Vec<u16>,
    pub queues: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxPlacement {
    pub name: &'static str,
    pub ports: Vec<u16>,
    pub first_queue: u32,
    pub queues: u32,
}

impl RxPlacement {
    pub fn queue_ids(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.queues).filter_map(|offset| self.first_queue.checked_add(offset))
    }

    pub fn owns(&self, queue: u32) -> bool {
        self.queue_ids().any(|owned| owned == queue)
    }

    pub fn needs_rss_context(&self) -> bool {
        self.queues > 1
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePlan {
    rx: Vec<RxPlacement>,
    queue_base: u32,
    tx_base: u32,
    tx_queues: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlanError {
    EmptyGroup(&'static str),
    NoPorts(&'static str),
    DuplicatePort { port: u16, groups: (&'static str, &'static str) },
    TooManyQueues,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::EmptyGroup(name) => {
                write!(f, "receive group `{name}` asks for zero queues")
            }
            PlanError::NoPorts(name) => write!(f, "receive group `{name}` has no ports"),
            PlanError::DuplicatePort { port, groups } => write!(
                f,
                "port {port} is claimed by both `{}` and `{}`; a port can only be steered to one \
                 group",
                groups.0, groups.1
            ),
            PlanError::TooManyQueues => write!(f, "the queue layout overflows a u32"),
        }
    }
}

impl std::error::Error for PlanError {}

impl QueuePlan {
    pub fn new(
        groups: Vec<RxGroup>,
        tx_queues: u32,
        queue_base: u32,
    ) -> Result<Self, PlanError> {
        let mut rx = Vec::with_capacity(groups.len());
        let mut next = queue_base;
        for group in groups {
            if group.queues == 0 {
                return Err(PlanError::EmptyGroup(group.name));
            }
            if group.ports.is_empty() {
                return Err(PlanError::NoPorts(group.name));
            }
            for port in &group.ports {
                if let Some(other) = rx
                    .iter()
                    .find(|placed: &&RxPlacement| placed.ports.contains(port))
                {
                    return Err(PlanError::DuplicatePort {
                        port: *port,
                        groups: (other.name, group.name),
                    });
                }
            }
            let first_queue = next;
            next = next.checked_add(group.queues).ok_or(PlanError::TooManyQueues)?;
            rx.push(RxPlacement {
                name: group.name,
                ports: group.ports,
                first_queue,
                queues: group.queues,
            });
        }
        next.checked_add(tx_queues).ok_or(PlanError::TooManyQueues)?;
        Ok(Self {
            rx,
            queue_base,
            tx_base: next,
            tx_queues,
        })
    }

    pub fn receive_groups(&self) -> &[RxPlacement] {
        &self.rx
    }

    pub fn group(&self, name: &str) -> Option<&RxPlacement> {
        self.rx.iter().find(|placed| placed.name == name)
    }

    pub fn tx_base(&self) -> u32 {
        self.tx_base
    }

    pub fn queue_base(&self) -> u32 {
        self.queue_base
    }

    pub fn total_queues(&self) -> u32 {
        self.tx_base.saturating_add(self.tx_queues)
    }

    pub fn owns_queue(&self, queue: u32) -> bool {
        (self.queue_base..self.total_queues()).contains(&queue)
    }

    pub fn owner_of(&self, queue: u32) -> Option<&RxPlacement> {
        self.rx.iter().find(|placed| placed.owns(queue))
    }

    pub fn steering(&self) -> impl Iterator<Item = (u16, u32)> + '_ {
        self.rx
            .iter()
            .flat_map(|placed| placed.ports.iter().map(|port| (*port, placed.first_queue)))
    }
}


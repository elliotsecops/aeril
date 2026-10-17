//! Aeril — un terminal donde el agente de IA es un participante nativo.
//!
//! Fase 0 (ensayo de viabilidad): un thread completo y recuperable —
//! editar → snapshot → revisar → comentar → feedback → reanudar tras crash —
//! con presupuestos de arranque/RAM/tamaño medidos en la laptop objetivo.

pub mod agent;
pub mod bench;
pub mod checkpoint;
pub mod cli;
pub mod crash;
pub mod error;
pub mod export;
pub mod git;
pub mod ids;
pub mod profile;
pub mod recovery;
pub mod review;
pub mod session;
pub mod store;
pub mod timeutil;
pub mod tui;
pub mod util;

//! Voice integration primitives. The application must persist authority/progress and
//! validate live provider gates before mounting a relay; these are not an HTTP API.
pub mod client;
pub mod controller;
pub mod next_step;
pub mod pre_speech;
pub mod protocol;

use std::sync::{Arc, Barrier};

use actix::prelude::*;

use super::Order;

// Order Taker
type FilePath = String;
#[derive(Message)]
#[rtype(result = "()")]
pub struct TakeOrders(pub FilePath);

// Order Handler
#[derive(Message)]
#[rtype(result = "()")]
pub struct HandleOrder(pub Order);

#[derive(Message)]
#[rtype(result = "()")]
pub struct WaitStop(pub Option<Arc<Barrier>>);

// Point Storage
#[derive(Message)]
#[rtype(result = "Result<(),String>")]
pub struct UsePoints(pub usize);

#[derive(Message)]
#[rtype(result = "Result<(),String>")]
pub struct FillPoints(pub usize);

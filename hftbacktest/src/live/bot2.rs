use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

use bincode::{
    config,
    error::{DecodeError, EncodeError},
};
use chrono::Utc;
use iceoryx2::{
    port::{
        publisher::{Publisher, PublisherLoanError, PublisherSendError},
        subscriber::{Subscriber, SubscriberReceiveError},
    },
    prelude::{zero_copy, Iox2, Iox2Event, Service, ServiceName},
    sample::Sample,
    service::port_factory::publish_subscribe::PortFactory,
};
use thiserror::Error;
use tracing::{debug, error};

use crate::{
    depth::{L2MarketDepth, MarketDepth},
    live::Asset,
    prelude::{OrderId, OrderRequest, WaitOrderResponse},
    types::{
        Bot,
        BuildError,
        Event,
        LiveError as ErrorEvent,
        LiveError,
        LiveEvent,
        OrdType,
        Order,
        Request,
        Side,
        StateValues,
        Status,
        TimeInForce,
        LOCAL_ASK_DEPTH_EVENT,
        LOCAL_BID_DEPTH_EVENT,
        LOCAL_BUY_TRADE_EVENT,
        LOCAL_SELL_TRADE_EVENT,
    },
};

#[repr(C)]
#[derive(Debug)]
pub struct BinPayload {
    pub data: [u8; 1024],
    pub len: usize,
}

impl Default for BinPayload {
    fn default() -> Self {
        Self {
            data: [0; 1024],
            len: 0,
        }
    }
}

#[derive(Error, Debug)]
pub enum PubSubError {
    #[error("{0:?}")]
    SubscriberReceive(#[from] SubscriberReceiveError),
    #[error("{0:?}")]
    PublisherLoan(#[from] PublisherLoanError),
    #[error("{0:?}")]
    PublisherSend(#[from] PublisherSendError),
    #[error("{0:?}")]
    Decode(#[from] DecodeError),
    #[error("{0:?}")]
    Encode(#[from] EncodeError),
}

pub struct IceoryxPubSub {
    _pub_factory: PortFactory<zero_copy::Service, Request>,
    _sub_factory: PortFactory<zero_copy::Service, LiveEvent>,
    publisher: Publisher<zero_copy::Service, Request>,
    subscriber: Subscriber<zero_copy::Service, LiveEvent>,
}

impl IceoryxPubSub {
    pub fn new(name: &str) -> Result<IceoryxPubSub, anyhow::Error> {
        let to_bot = ServiceName::new(&format!("{}/ToBot", name))?;
        let sub_factory = zero_copy::Service::new(&to_bot)
            .publish_subscribe()
            .max_publishers(1)
            .max_subscribers(1000)
            .open_or_create::<LiveEvent>()?;

        let subscriber = sub_factory.subscriber().create()?;

        let from_bot = ServiceName::new(&format!("{}/FromBot", name))?;
        let pub_factory = zero_copy::Service::new(&from_bot)
            .publish_subscribe()
            .max_publishers(1000)
            .max_subscribers(1)
            .open_or_create::<Request>()?;

        let publisher = pub_factory.publisher().create()?;

        Ok(IceoryxPubSub {
            _pub_factory: pub_factory,
            _sub_factory: sub_factory,
            publisher,
            subscriber,
        })
    }

    pub fn receive(
        &self,
    ) -> Result<Option<Sample<LiveEvent, iceoryx2::service::zero_copy::Service>>, PubSubError> {
        Ok(self.subscriber.receive()?)
    }

    pub fn send(&self, req: Request) -> Result<(), PubSubError> {
        let sample = self.publisher.loan_uninit()?.write_payload(req);
        sample.send()?;
        Ok(())
    }
}

#[derive(Error, Debug)]
pub enum BotError {
    #[error("order id already exists")]
    OrderIdExist,
    #[error("asset not found")]
    AssetNotFound,
    #[error("order not found")]
    OrderNotFound,
    #[error("order status is invalid")]
    InvalidOrderStatus,
    #[error("{0}")]
    Custom(String),
    #[error("{0:?}")]
    PubSub(#[from] PubSubError),
}

pub type ErrorHandler = Box<dyn Fn(ErrorEvent) -> Result<(), BotError>>;
pub type OrderRecvHook = Box<dyn Fn(&Order, &Order) -> Result<(), BotError>>;

pub type DepthBuilder<MD> = Box<dyn FnMut(&Asset) -> MD>;

/// Live [`LiveBot`] builder.
pub struct LiveBotBuilder2<MD> {
    assets: Vec<(String, Asset)>,
    error_handler: Option<ErrorHandler>,
    order_hook: Option<OrderRecvHook>,
    depth_builder: Option<DepthBuilder<MD>>,
    last_trades_capacity: usize,
}

impl<MD> LiveBotBuilder2<MD> {
    /// Adds an asset.
    ///
    /// * `name` - Name of the [`Connector`], which is registered by
    ///            [`register()`](`LiveBotBuilder::register()`), through which this asset will be
    ///            traded.
    /// * `symbol` - Symbol of the asset. You need to check with the [`Connector`] which symbology
    ///              is used.
    /// * `tick_size` - The minimum price fluctuation.
    /// * `lot_size` -  The minimum trade size.
    pub fn add(self, name: &str, symbol: &str, tick_size: f64, lot_size: f64) -> Self {
        Self {
            assets: {
                let asset_no = self.assets.len();
                let mut assets = self.assets;
                assets.push((
                    name.to_string(),
                    Asset {
                        asset_no,
                        symbol: symbol.to_string(),
                        tick_size,
                        lot_size,
                    },
                ));
                assets
            },
            ..self
        }
    }

    /// Registers the error handler to deal with an error from connectors.
    pub fn error_handler<Handler>(self, handler: Handler) -> Self
    where
        Handler: Fn(LiveError) -> Result<(), BotError> + 'static,
    {
        Self {
            error_handler: Some(Box::new(handler)),
            ..self
        }
    }

    /// Registers the order response receive hook.
    pub fn order_recv_hook<Hook>(self, hook: Hook) -> Self
    where
        Hook: Fn(&Order, &Order) -> Result<(), BotError> + 'static,
    {
        Self {
            order_hook: Some(Box::new(hook)),
            ..self
        }
    }

    /// Sets [`MarketDepth`] build function.
    pub fn depth<Builder>(self, builder: Builder) -> Self
    where
        Builder: Fn(&Asset) -> MD + 'static,
    {
        Self {
            depth_builder: Some(Box::new(builder)),
            ..self
        }
    }

    /// Sets the length of market trades to be stored in the local processor. The default value is
    /// `0`.
    pub fn last_trades_capacity(self, last_trades_capacity: usize) -> Self {
        Self {
            last_trades_capacity,
            ..self
        }
    }

    /// Builds a live [`LiveBot`] based on the registered connectors and assets.
    pub fn build(self) -> Result<LiveBot2<MD>, BuildError> {
        let mut dup = HashSet::new();
        let mut tmp_pubsub: HashMap<String, Rc<IceoryxPubSub>> = HashMap::new();
        let mut pubsub = Vec::new();
        for (name, asset_info) in self.assets.iter() {
            if !dup.insert(format!("{}/{}", name, asset_info.symbol)) {
                Err(BuildError::Duplicate(
                    name.clone(),
                    asset_info.symbol.clone(),
                ))?;
            }

            match tmp_pubsub.entry(name.clone()) {
                Entry::Occupied(entry) => {
                    let ps = entry.get().clone();
                    pubsub.push(ps);
                }
                Entry::Vacant(entry) => {
                    let ps = Rc::new(IceoryxPubSub::new(name)?);
                    entry.insert(ps.clone());
                    pubsub.push(ps);
                }
            }
        }

        let mut depth_builder = self
            .depth_builder
            .ok_or(BuildError::BuilderIncomplete("depth"))?;
        let depth = self
            .assets
            .iter()
            .map(|(_, asset_info)| depth_builder(asset_info))
            .collect();

        let orders = self.assets.iter().map(|_| HashMap::new()).collect();
        let state = self.assets.iter().map(|_| Default::default()).collect();
        let trade = self
            .assets
            .iter()
            .map(|_| Vec::with_capacity(self.last_trades_capacity))
            .collect();
        let last_feed_latency = self.assets.iter().map(|_| None).collect();
        let last_order_latency = self.assets.iter().map(|_| None).collect();

        Ok(LiveBot2 {
            pubsub,
            depth,
            orders,
            state,
            assets: self.assets,
            trade,
            last_trades_capacity: self.last_trades_capacity,
            error_handler: self.error_handler,
            order_hook: self.order_hook,
            last_feed_latency,
            last_order_latency,
        })
    }
}

/// A live trading bot.
///
/// Provides the same interface as the backtesters in [`backtest`](`crate::backtest`).
///
/// ```
/// use hftbacktest::{live::LiveBot, prelude::HashMapMarketDepth};
///
/// let mut hbt = LiveBot2::builder()
///     .add("connector_name", "symbol", tick_size, lot_size)
///     .depth(|asset| HashMapMarketDepth::new(asset.tick_size, asset.lot_size))
///     .build()
///     .unwrap();
/// ```
pub struct LiveBot2<MD> {
    pubsub: Vec<Rc<IceoryxPubSub>>,
    depth: Vec<MD>,
    orders: Vec<HashMap<OrderId, Order>>,
    trade: Vec<Vec<Event>>,
    last_trades_capacity: usize,
    assets: Vec<(String, Asset)>,
    error_handler: Option<ErrorHandler>,
    order_hook: Option<OrderRecvHook>,
    last_feed_latency: Vec<Option<(i64, i64)>>,
    last_order_latency: Vec<Option<(i64, i64, i64)>>,
    state: Vec<StateValues>,
}

impl<MD> LiveBot2<MD>
where
    MD: MarketDepth + L2MarketDepth,
{
    /// Builder to construct [`LiveBot2`] instances.
    pub fn builder() -> LiveBotBuilder2<MD> {
        LiveBotBuilder2 {
            assets: Vec::new(),
            error_handler: None,
            order_hook: None,
            depth_builder: None,
            last_trades_capacity: 0,
        }
    }

    fn process_event<const WAIT_NEXT_FEED: bool>(
        &mut self,
        now: Instant,
        duration: i64,
        wait_order_response: WaitOrderResponse,
    ) -> Result<bool, BotError> {
        loop {
            let elapsed = now.elapsed().as_nanos() as i64;
            if elapsed > duration {
                return Ok(true);
            }

            let mut no_data = 0;
            for subscriber in &self.pubsub {
                let Some(ref ev) = subscriber.receive()? else {
                    no_data += 1;
                    continue;
                };

                match ev.payload() {
                    LiveEvent::FeedBatch { asset_no, events } => {
                        let asset_no = *asset_no;
                        for event in events {
                            *unsafe { self.last_feed_latency.get_unchecked_mut(asset_no) } =
                                Some((event.exch_ts, event.local_ts));
                            if event.is(LOCAL_BID_DEPTH_EVENT) {
                                let depth = unsafe { self.depth.get_unchecked_mut(asset_no) };
                                depth.update_bid_depth(event.px, event.qty, event.exch_ts);
                            } else if event.is(LOCAL_ASK_DEPTH_EVENT) {
                                let depth = unsafe { self.depth.get_unchecked_mut(asset_no) };
                                depth.update_ask_depth(event.px, event.qty, event.exch_ts);
                            } else if (event.is(LOCAL_BUY_TRADE_EVENT)
                                || event.is(LOCAL_SELL_TRADE_EVENT))
                                && self.last_trades_capacity > 0
                            {
                                let trade = unsafe { self.trade.get_unchecked_mut(asset_no) };
                                trade.push(event.clone());
                            }
                        }
                        if WAIT_NEXT_FEED {
                            return Ok(true);
                        }
                    }
                    LiveEvent::Feed { asset_no, event } => {
                        let asset_no = *asset_no;
                        *unsafe { self.last_feed_latency.get_unchecked_mut(asset_no) } =
                            Some((event.exch_ts, event.local_ts));
                        if event.is(LOCAL_BID_DEPTH_EVENT) {
                            let depth = unsafe { self.depth.get_unchecked_mut(asset_no) };
                            depth.update_bid_depth(event.px, event.qty, event.exch_ts);
                        } else if event.is(LOCAL_ASK_DEPTH_EVENT) {
                            let depth = unsafe { self.depth.get_unchecked_mut(asset_no) };
                            depth.update_ask_depth(event.px, event.qty, event.exch_ts);
                        } else if (event.is(LOCAL_BUY_TRADE_EVENT)
                            || event.is(LOCAL_SELL_TRADE_EVENT))
                            && self.last_trades_capacity > 0
                        {
                            let trade = unsafe { self.trade.get_unchecked_mut(asset_no) };
                            trade.push(event.clone());
                        }
                    }
                    LiveEvent::Order { asset_no, order } => {
                        let asset_no = *asset_no;
                        debug!(%asset_no, ?order, "Event::Order");
                        let received_order_resp = match wait_order_response {
                            WaitOrderResponse::Any => true,
                            WaitOrderResponse::Specified {
                                asset_no: wait_order_asset_no,
                                order_id: wait_order_id,
                            } if wait_order_id == order.order_id
                                && wait_order_asset_no == asset_no =>
                            {
                                true
                            }
                            _ => false,
                        };
                        *unsafe { self.last_order_latency.get_unchecked_mut(asset_no) } = Some((
                            order.local_timestamp,
                            order.exch_timestamp,
                            Utc::now().timestamp_nanos_opt().unwrap(),
                        ));
                        match self
                            .orders
                            .get_mut(asset_no)
                            .ok_or(BotError::AssetNotFound)?
                            .entry(order.order_id)
                        {
                            Entry::Occupied(mut entry) => {
                                let ex_order = entry.get_mut();
                                if let Some(hook) = self.order_hook.as_mut() {
                                    hook(ex_order, &order)?;
                                }
                                if order.exch_timestamp >= ex_order.exch_timestamp {
                                    if ex_order.status == Status::Canceled
                                        || ex_order.status == Status::Expired
                                        || ex_order.status == Status::Filled
                                    {
                                        // Ignores the update since the current status is the final status.
                                    } else {
                                        ex_order.update(&order);
                                    }
                                }
                            }
                            Entry::Vacant(entry) => {
                                error!(
                                    %asset_no,
                                    ?order,
                                    "Bot received an unmanaged order. \
                                    This should be handled by a Connector."
                                );
                                entry.insert(order.clone());
                            }
                        }
                        if received_order_resp {
                            return Ok(true);
                        }
                    }
                    LiveEvent::Position { asset_no, qty } => {
                        unsafe { self.state.get_unchecked_mut(*asset_no) }.position = *qty;
                    }
                    LiveEvent::Error(error) => {
                        if let Some(handler) = self.error_handler.as_mut() {
                            handler(error.clone())?;
                        }
                    }
                }
            }
            if self.pubsub.len() == no_data {
                return Ok(false);
            }
        }
    }

    fn elapse_<const WAIT_NEXT_FEED: bool>(
        &mut self,
        duration: i64,
        wait_order_response: WaitOrderResponse,
    ) -> Result<bool, BotError> {
        let now = Instant::now();
        let mut remaining_duration = duration;

        loop {
            let cycle_time = Duration::from_nanos(1000.min(remaining_duration as u64));
            match Iox2::wait(cycle_time) {
                Iox2Event::Tick => {
                    if self.process_event::<WAIT_NEXT_FEED>(now, duration, wait_order_response)? {
                        return Ok(true);
                    }
                }
                Iox2Event::TerminationRequest => {
                    return Ok(false);
                }
                Iox2Event::InterruptSignal => {
                    return Ok(false);
                }
            }
            let elapsed = now.elapsed().as_nanos() as i64;
            if elapsed > duration {
                return Ok(true);
            }
            remaining_duration = duration - elapsed;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_order(
        &mut self,
        asset_no: usize,
        order_id: u64,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
        side: Side,
    ) -> Result<bool, BotError> {
        let orders = self
            .orders
            .get_mut(asset_no)
            .ok_or(BotError::AssetNotFound)?;
        if orders.contains_key(&order_id) {
            return Err(BotError::OrderIdExist);
        }
        let tick_size = self.assets.get(asset_no).unwrap().1.tick_size;
        let order = Order {
            order_id,
            price_tick: (price / tick_size).round() as i64,
            qty,
            leaves_qty: qty,
            tick_size,
            side,
            time_in_force,
            order_type,
            status: Status::New,
            local_timestamp: Utc::now().timestamp_nanos_opt().unwrap(),
            req: Status::New,
            exec_price_tick: 0,
            exch_timestamp: 0,
            exec_qty: 0.0,
            // Invalid information
            q: Box::new(()),
            maker: false,
        };
        let order_id = order.order_id;
        orders.insert(order_id, order.clone());

        let publisher = self.pubsub.get(asset_no).ok_or(BotError::AssetNotFound)?;

        publisher.send(Request::Order { asset_no, order })?;

        if wait {
            // fixme: timeout should be specified by the argument.
            return self.wait_order_response(asset_no, order_id, 60_000_000_000);
        }
        Ok(true)
    }
}

impl<MD> Bot<MD> for LiveBot2<MD>
where
    MD: MarketDepth + L2MarketDepth,
{
    type Error = BotError;

    #[inline]
    fn current_timestamp(&self) -> i64 {
        Utc::now().timestamp_nanos_opt().unwrap()
    }

    #[inline]
    fn num_assets(&self) -> usize {
        self.state.len()
    }

    #[inline]
    fn position(&self, asset_no: usize) -> f64 {
        self.state_values(asset_no).position
    }

    #[inline]
    fn state_values(&self, asset_no: usize) -> &StateValues {
        // todo: implement the missing fields. Trade values need to be changed to a rolling manner,
        //       unlike the current Python implementation, to support live trading.
        self.state.get(asset_no).unwrap()
    }

    #[inline]
    fn depth(&self, asset_no: usize) -> &MD {
        self.depth.get(asset_no).unwrap()
    }

    #[inline]
    fn last_trades(&self, asset_no: usize) -> &[Event] {
        self.trade.get(asset_no).unwrap().as_slice()
    }

    fn clear_last_trades(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(asset_no) => {
                self.trade.get_mut(asset_no).unwrap().clear();
            }
            None => {
                for asset_no in 0..self.trade.len() {
                    self.trade.get_mut(asset_no).unwrap().clear();
                }
            }
        }
    }

    #[inline]
    fn orders(&self, asset_no: usize) -> &HashMap<OrderId, Order> {
        self.orders.get(asset_no).unwrap()
    }

    #[inline]
    fn submit_buy_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        self.submit_order(
            asset_no,
            order_id,
            price,
            qty,
            time_in_force,
            order_type,
            wait,
            Side::Buy,
        )
    }

    #[inline]
    fn submit_sell_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        self.submit_order(
            asset_no,
            order_id,
            price,
            qty,
            time_in_force,
            order_type,
            wait,
            Side::Sell,
        )
    }

    fn submit_order(
        &mut self,
        asset_no: usize,
        order: OrderRequest,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        self.submit_order(
            asset_no,
            order.order_id,
            order.price,
            order.qty,
            order.time_in_force,
            order.order_type,
            wait,
            order.side,
        )
    }

    #[inline]
    fn cancel(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let orders = self
            .orders
            .get_mut(asset_no)
            .ok_or(BotError::AssetNotFound)?;
        let order = orders.get_mut(&order_id).ok_or(BotError::OrderNotFound)?;
        if !order.cancellable() {
            return Err(BotError::InvalidOrderStatus);
        }
        order.req = Status::Canceled;
        order.local_timestamp = Utc::now().timestamp_nanos_opt().unwrap();

        let publisher = self.pubsub.get(asset_no).ok_or(BotError::AssetNotFound)?;

        publisher.send(Request::Order {
            asset_no,
            order: order.clone(),
        })?;

        if wait {
            // fixme: timeout should be specified by the argument.
            return self.wait_order_response(asset_no, order_id, 60_000_000_000);
        }
        Ok(true)
    }

    #[inline]
    fn clear_inactive_orders(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(an) => {
                if let Some(orders) = self.orders.get_mut(an) {
                    orders.retain(|_, order| order.active());
                }
            }
            None => {
                for orders in self.orders.iter_mut() {
                    orders.retain(|_, order| order.active());
                }
            }
        }
    }

    #[inline]
    fn wait_order_response(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        timeout: i64,
    ) -> Result<bool, Self::Error> {
        self.elapse_::<false>(timeout, WaitOrderResponse::Specified { asset_no, order_id })
    }

    #[inline]
    fn wait_next_feed(
        &mut self,
        include_order_resp: bool,
        timeout: i64,
    ) -> Result<bool, Self::Error> {
        if include_order_resp {
            self.elapse_::<true>(timeout, WaitOrderResponse::Any)
        } else {
            self.elapse_::<true>(timeout, WaitOrderResponse::None)
        }
    }

    #[inline]
    fn elapse(&mut self, duration: i64) -> Result<bool, Self::Error> {
        self.elapse_::<false>(duration, WaitOrderResponse::None)
    }

    #[inline]
    fn elapse_bt(&mut self, _duration: i64) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn feed_latency(&self, asset_no: usize) -> Option<(i64, i64)> {
        *self.last_feed_latency.get(asset_no).unwrap()
    }

    fn order_latency(&self, asset_no: usize) -> Option<(i64, i64, i64)> {
        *self.last_order_latency.get(asset_no).unwrap()
    }
}

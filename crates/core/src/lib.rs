use std::{
    any::{Any, TypeId},
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use parking_lot::{Condvar, Mutex};

struct PendingQueue {
    queue: Mutex<VecDeque<(TypeId, Box<dyn Any + Send>)>>,
    condvar: Condvar,
}

pub struct EventManager {
    registry: HashMap<TypeId, Box<dyn Dispatch>>,
    pending: Arc<PendingQueue>,
}

impl EventManager {
    pub fn new() -> Self {
        Self {
            registry: Default::default(),
            pending: Arc::new(PendingQueue {
                queue: Mutex::new(VecDeque::new()),
                condvar: Condvar::new(),
            }),
        }
    }

    pub fn register_component<C: Component>(&mut self, component: C) -> RegisteredComponent<C> {
        RegisteredComponent::new(component)
    }

    pub fn subscribe_event<E: Event, C: EventHandler<E> + Component>(
        &mut self,
        component: RegisteredComponent<C>,
    ) {
        self.get_event_handlers::<E>().insert_handler(component);
    }

    pub fn send_event<E: Event>(&mut self, event: E) {
        self.get_event_handlers::<E>().send_event(event);
    }

    pub fn register_event<E: Event + Send>(&mut self) {
        if self.registry.contains_key(&TypeId::of::<E>()) {
            panic!("Event was already registered!")
        }
        self.registry
            .insert(TypeId::of::<E>(), Box::new(EventHandlers::<E>::new()));
    }

    /// Returns a handle to this [`EventManager`]'s inbox. Clone it and give
    /// it to other threads or other [`EventManager`]s. Calling
    /// [`EventManagerHandle::send_event`] on the handle queues an event to be
    /// dispatched here on the next [`drain_pending`] / [`wait_and_drain_pending`].
    pub fn handle(&self) -> EventManagerHandle {
        EventManagerHandle {
            pending: Arc::clone(&self.pending),
        }
    }

    /// Drains and dispatches all cross-thread events queued so far.
    /// Returns immediately if the queue is empty.
    pub fn drain_pending(&mut self) {
        let drained = {
            let mut q = self.pending.queue.lock();
            std::mem::take(&mut *q)
        };
        for (type_id, event) in drained {
            if let Some(h) = self.registry.get_mut(&type_id) {
                h.dispatch_boxed(event);
            }
        }
    }

    /// Blocks until at least one cross-thread event is queued, then drains
    /// and dispatches all pending events.
    pub fn wait_and_drain_pending(&mut self) {
        let drained = {
            let mut q = self.pending.queue.lock();
            while q.is_empty() {
                self.pending.condvar.wait(&mut q);
            }
            std::mem::take(&mut *q)
        };
        for (type_id, event) in drained {
            if let Some(h) = self.registry.get_mut(&type_id) {
                h.dispatch_boxed(event);
            }
        }
    }

    /// Drain all cross-thread events of type `E` from the pending queue and
    /// return them as an owned `Vec`. Events of other types remain in the queue.
    /// Returns immediately (non-blocking).
    pub fn drain_typed<E: Event + Send>(&mut self) -> Vec<E> {
        let type_id = TypeId::of::<E>();
        let mut q = self.pending.queue.lock();
        if q.is_empty() {
            return vec![];
        }
        let mut taken = Vec::new();
        let mut remaining = VecDeque::new();
        while let Some(item) = q.pop_front() {
            if item.0 == type_id {
                taken.push(*item.1.downcast::<E>().unwrap());
            } else {
                remaining.push_back(item);
            }
        }
        *q = remaining;
        taken
    }

    /// Block until at least one cross-thread event is queued.
    /// Does **not** dispatch or consume any events — call [`drain_typed`] or
    /// [`drain_pending`] / [`wait_and_drain_pending`] afterward.
    pub fn wait_for_pending(&self) {
        let mut q = self.pending.queue.lock();
        while q.is_empty() {
            self.pending.condvar.wait(&mut q);
        }
    }

    fn get_event_handlers<E: Event>(&mut self) -> &mut EventHandlers<E> {
        let type_id = TypeId::of::<E>();
        self.registry
            .get_mut(&type_id)
            .expect("Event was not registered!")
            .as_any_mut()
            .downcast_mut()
            .unwrap()
    }
}

/// A cloneable, `Send` reference to another [`EventManager`]'s inbox.
/// Obtain one via [`EventManager::handle`].
///
/// Any thread (or another [`EventManager`]) holding a handle can push events
/// into the target manager's queue. The target dispatches them when it calls
/// [`EventManager::drain_pending`] or [`EventManager::wait_and_drain_pending`].
#[derive(Clone)]
pub struct EventManagerHandle {
    pending: Arc<PendingQueue>,
}

impl EventManagerHandle {
    /// Queue an event for the target [`EventManager`]. Non-blocking.
    pub fn send_event<E: Event + Send>(&self, event: E) {
        let mut q = self.pending.queue.lock();
        q.push_back((TypeId::of::<E>(), Box::new(event)));
        self.pending.condvar.notify_one();
    }
}

trait Dispatch: Any + Send {
    fn dispatch_boxed(&mut self, event: Box<dyn Any + Send>);
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub trait Event: 'static {}
pub trait EventHandler<E: Event>: 'static + Send {
    fn handle(&mut self, event: E) -> E;
}

struct EventHandlers<E: Event> {
    handlers: Vec<Box<dyn EventHandler<E> + Send>>,
}

impl<E: Event> EventHandlers<E> {
    pub fn new() -> Self {
        Self { handlers: vec![] }
    }
    pub fn send_event(&mut self, mut event: E) {
        for handler in &mut self.handlers {
            event = handler.handle(event)
        }
    }
    pub fn insert_handler(&mut self, handler: impl EventHandler<E> + Send + 'static) {
        self.handlers.push(Box::new(handler));
    }
}

impl<E: Event + Send> Dispatch for EventHandlers<E> {
    fn dispatch_boxed(&mut self, event: Box<dyn Any + Send>) {
        self.send_event(*event.downcast::<E>().unwrap());
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub struct RegisteredComponent<T: Component>(Arc<Mutex<T>>);
impl<T: Component> RegisteredComponent<T> {
    fn new(component: T) -> Self {
        Self(Arc::new(Mutex::new(component)))
    }
}

impl<E: Event, T: Component + EventHandler<E>> EventHandler<E> for RegisteredComponent<T> {
    fn handle(&mut self, event: E) -> E {
        self.0.lock().handle(event)
    }
}

pub trait Component {
    fn build(&mut self, app: &mut EventManager);
}

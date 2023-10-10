type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static>>;

# SQLx Sessions for Rocket.rs Version 0.5.0-dev

Adding long term stored cookie-based sessions simular too Flask-SessionStore to a rocket application is extremely simple:

```rust
#![feature(proc_macro_hygiene, decl_macro)]
#[macro_use] extern crate rocket;

use rocket_sqlxsession::{SqlxSessionFairing, SQLxSession};

fn main() {
    rocket::ignite()
        .attach(SqlxSessionFairing::new()
            .with_database("databasename")
            .with_username("username")
            .with_password("password")
            .with_host("localhost")
            .with_port("5432"))
        .mount("/", routes![index])
        .launch();
}

#[get("/")]
fn index(sqlxsession: SQLxSession) -> String {
    let mut count: usize = sqlxsession.get("count").unwrap_or(0);
    count += 1;
    sqlxsession.set("count", count);

    format!("{} visits", count)
}
```

Anything serializable can be stored in the SQL session, just make sure to unpack it to the right type.

The SQL session driver internally converts data to a string which is easier saved as a whole session via serdes json.

This library is based on [sqlx_async_sessions](https://crates.io/crates/async-sqlx-session) and [rocket_sessions](https://crates.io/crates/rocket_session)
and based on how Flask-SessionStore works.

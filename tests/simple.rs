use log::info;
use serde_json::to_string_pretty;

use cozo::Db;
use cozorocks::DbBuilder;

fn create_db(name: &str, destroy_on_exit: bool) -> Db {
    let builder = DbBuilder::default()
        .path(name)
        .create_if_missing(true)
        .destroy_on_exit(destroy_on_exit);
    Db::build(builder).unwrap()
}

fn init_logger() {
    let _ = env_logger::builder().is_test(true).try_init();
}

fn test_send_sync<T: Send + Sync>(_: &T) {}

#[test]
fn simple() {
    init_logger();
    let db = create_db("_test_db", true);
    test_send_sync(&db);
    assert!(db.current_schema().unwrap().as_array().unwrap().is_empty());
    db.run_tx_attributes(
        r#"
        put person {
            id: string identity,
            first_name: string index,
            last_name: string index,
            age: int,
            friend: ref many,
            weight: float,
        }
    "#,
    )
    .unwrap();
    assert_eq!(db.current_schema().unwrap().as_array().unwrap().len(), 6);
    info!(
        "{}",
        to_string_pretty(&db.current_schema().unwrap()).unwrap()
    );
    db.run_tx_triples(
        r#"
        {
            _temp_id: "alice",
            person.first_name: "Alice",
            person.age: 7,
            person.last_name: "Amorist",
            person.id: "alice_amorist",
            person.weight: 25,
            person.friend: "eve"
        }
        {
            _temp_id: "bob",
            person.first_name: "Bob",
            person.age: 70,
            person.last_name: "Wonderland",
            person.id: "bob_wonderland",
            person.weight: 100,
            person.friend: "alice"
        }
        {
            _temp_id: "eve",
            person.first_name: "Eve",
            person.age: 18,
            person.last_name: "Faking",
            person.id: "eve_faking",
            person.weight: 50,
            person.friend: [
                "alice",
                "bob",
                {
                    person.first_name: "Charlie",
                    person.age: 22,
                    person.last_name: "Goodman",
                    person.id: "charlie_goodman",
                    person.weight: 120,
                    person.friend: "eve"
                }
            ]
        }
        {
            _temp_id: "david",
            person.first_name: "David",
            person.age: 7,
            person.last_name: "Dull",
            person.id: "david_dull",
            person.weight: 25,
            person.friend: {
                _temp_id: "george",
                person.first_name: "George",
                person.age: 7,
                person.last_name: "Geomancer",
                person.id: "george_geomancer",
                person.weight: 25,
                person.friend: "george"},
        }
    "#,
    )
    .unwrap();
    let query = r#"
    friend_of_friend[?a, ?b] := [?a person.friend ?b];
    friend_of_friend[?a, ?b] := [?a person.friend ?c], friend_of_friend[?c, ?b];

    ?[?a, ?n] := [?alice person.first_name "Alice"],
                 not friend_of_friend[?alice, ?a],
                 [?a person.first_name ?n];

    :limit 1;
    :out {friend: ?a[person.first_name as first_name,
                     person.last_name as last_name]};
    :sort -?n;
    "#;

    let ret = db.run_script(query).unwrap();
    let res = to_string_pretty(&ret).unwrap();
    info!("{}", res);
}
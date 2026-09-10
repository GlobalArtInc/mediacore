use super::*;

use std::collections::HashMap;

const KEY: &str = "devkey";
const SECRET: &str = "devsecretdevsecretdevsecretdevsecret";

fn minter() -> Tokens {
    Tokens::mint(KEY.to_owned(), SECRET.to_owned(), Duration::from_secs(600)).expect("a minter")
}

fn keys() -> HashMap<String, String> {
    HashMap::from([(KEY.to_owned(), SECRET.to_owned())])
}

#[test]
fn a_minted_token_lets_a_bot_into_the_room_it_names() {
    let token = minter().issue(0, "bot-0", "load-0").expect("a token");
    let claims = mediacore_auth::verify_join(&token, &keys()).expect("the server takes it");

    assert_eq!(claims.identity(), "bot-0");
    assert_eq!(claims.video.room, "load-0");
    assert!(claims.video.can_publish());
    assert!(claims.video.can_subscribe());
}

#[test]
fn every_bot_gets_a_token_of_its_own() {
    let tokens = minter();
    let first = tokens.issue(0, "bot-0", "load-0").expect("a token");
    let second = tokens.issue(1, "bot-1", "load-1").expect("a token");

    assert_ne!(first, second);
    assert_eq!(tokens.capacity(), None);
}

#[test]
fn minting_without_a_secret_is_refused_before_anything_connects() {
    // Matched rather than unwrapped, because `Tokens` deliberately has no
    // `Debug` to unwrap through: it holds the api secret.
    let error = match Tokens::mint(KEY.to_owned(), String::new(), Duration::from_secs(60)) {
        Ok(_) => panic!("a minter was built without a secret to sign with"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("--token"), "{error}");
}

#[test]
fn given_tokens_are_handed_out_in_the_order_they_arrived() {
    let tokens = Tokens::Given(vec!["one".to_owned(), "two".to_owned()]);

    assert_eq!(tokens.capacity(), Some(2));
    assert_eq!(tokens.issue(0, "bot-0", "load-0").unwrap(), "one");
    assert_eq!(tokens.issue(1, "bot-1", "load-0").unwrap(), "two");

    let error = tokens
        .issue(2, "bot-2", "load-0")
        .expect_err("only two were given");
    assert!(error.to_string().contains("2 were given"), "{error}");
}

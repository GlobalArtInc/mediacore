use super::*;
use mediacore_auth::VideoGrant;

fn claims(video: VideoGrant) -> Claims {
    Claims {
        video,
        ..Claims::default()
    }
}

#[test]
fn an_admin_token_reads_the_history_of_its_own_room_only() {
    let admin = claims(VideoGrant {
        room_admin: Some(true),
        room: "standup".to_owned(),
        ..VideoGrant::default()
    });

    assert!(reads(&admin, "standup"));
    assert!(
        !reads(&admin, "payroll"),
        "a token minted for one meeting read another meeting's attendance list"
    );
}

#[test]
fn the_unscoped_read_grant_reads_every_room() {
    let lister = claims(VideoGrant {
        room_list: Some(true),
        ..VideoGrant::default()
    });

    assert!(reads(&lister, "standup"));
    assert!(
        reads(&lister, "payroll"),
        "roomList already returns every room and the whole of every room record; refusing it \
             the meetings would be a lock on a door with no wall"
    );
}

#[test]
fn making_rooms_and_joining_them_are_not_ways_of_reading_them() {
    let creator = claims(VideoGrant {
        room_create: Some(true),
        room: "standup".to_owned(),
        ..VideoGrant::default()
    });
    let joiner = claims(VideoGrant {
        room_join: Some(true),
        room: "standup".to_owned(),
        ..VideoGrant::default()
    });

    assert!(
        !reads(&creator, "standup"),
        "roomCreate builds rooms and deletes them; it has never been a way to look inside one"
    );
    assert!(
        !reads(&joiner, "standup"),
        "roomJoin is the grant everybody in the meeting holds, and the attendance list is not \
             theirs to read"
    );
}

#[test]
fn every_endpoint_on_the_page_is_one_this_module_serves() {
    for endpoint in ENDPOINTS {
        assert!(
            [
                ROOM_SESSIONS_PATH,
                ROOM_SESSION_PATH,
                ROOMS_SESSIONS_PATH,
                SESSION_PATH,
                SESSION_RECORDINGS_PATH
            ]
            .contains(&endpoint.path),
            "`{}` is documented at a path this module does not serve",
            endpoint.path
        );

        let placeholder = format!("{{{}}}", endpoint.parameter.0);
        match endpoint.located {
            Where::Path => assert!(
                endpoint.path.contains(&placeholder),
                "`{}` documents a path parameter `{}` that its path does not carry",
                endpoint.operation,
                endpoint.parameter.0
            ),
            Where::Query => assert!(
                !endpoint.path.contains('{'),
                "`{}` documents a query parameter, so its path takes none",
                endpoint.operation
            ),
        }
    }

    assert_eq!(
        ENDPOINTS.len(),
        5,
        "an endpoint was added or removed and the page's table was not told"
    );
}

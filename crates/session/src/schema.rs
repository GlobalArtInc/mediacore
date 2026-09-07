//! The tables, and how they get there.
//!
//! **Why there is no migration tool.** This server owns four tables. Adding
//! sqlx or refinery to manage them would mean a build-time database or a
//! second binary in the image, a directory of files that has to travel with
//! the release, and a second way of being wrong about which version is
//! deployed -- for four tables that one person will change twice a year. So
//! the migrations are here, in the binary, in order, and the server applies
//! whichever of them the database has not got.
//!
//! **What upgrading looks like.** Start the new binary. On its first
//! connection it takes a Postgres advisory lock, reads `mediacore_schema`,
//! applies every migration with a higher version than it finds -- each in its
//! own transaction -- and releases the lock. The other two nodes do the same
//! thing a moment later, find nothing to apply and carry on; the lock is what
//! makes three simultaneous starts safe, and it is released by Postgres itself
//! if the node holding it dies mid-migration, so a crash during an upgrade
//! leaves the lock free and the half-applied migration rolled back with its
//! transaction.
//!
//! **What it does not do.** There is no down migration and no way to ask for
//! one. Going back a release means going back to a database from before the
//! upgrade, which is the honest answer for a schema that nobody is going to
//! reverse correctly under pressure; a migration is therefore written so that
//! the *previous* release can still run against it -- add columns, do not
//! rename them -- and the release after it removes what nothing reads any
//! more.
//!
//! **Where migration 4 breaks that rule, on purpose.** It renames the meeting
//! table to `mediacore_session` and drops the account column, so the release
//! before it cannot run against a database that has had it. That is a
//! deliberate exception taken while the only two databases in existence are
//! ours and are upgraded together: the word the schema used was the one word
//! in this server that disagreed with every other, and carrying the
//! disagreement forward for ever to protect a rollback nobody would take is a
//! worse trade than saying plainly that this one is a forward-only step.
//!
//! **Why every table is prefixed.** A deployment this size very often points
//! several services at one database. `session` and `recording` are names
//! somebody else will want; `mediacore_session` is not.

/// One step, and what it does.
pub(crate) struct Migration {
    pub version: i32,
    /// What it is for, in a few words. Written into `mediacore_schema` so that
    /// `select * from mediacore_schema` reads as a history rather than a list
    /// of numbers.
    pub name: &'static str,
    pub sql: &'static str,
}

/// The advisory lock every node takes before it looks at the schema.
///
/// A constant, and it has to be: two nodes computing different keys would both
/// think they held the lock. The value is the first eight bytes of the word
/// `mediacore` read as a big-endian integer, which is as good a way of picking
/// one number that nothing else in the database will pick as any other.
pub(crate) const MIGRATION_LOCK: i64 = 0x6D65_6469_6163_6F72;

/// The table the applied versions are written into.
///
/// Created outside the migration list, because it is the thing the list is
/// read against. `CREATE TABLE IF NOT EXISTS` rather than a version zero:
/// a database that already has it is the ordinary case, and the statement
/// says so.
pub(crate) const SCHEMA_TABLE: &str = "
CREATE TABLE IF NOT EXISTS mediacore_schema (
    version    integer PRIMARY KEY,
    name       text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
)";

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "meetings, attendance, recordings and the work that follows them",
        sql: "
CREATE TABLE IF NOT EXISTS mediacore_meeting (
    id               text PRIMARY KEY,
    room             text NOT NULL,
    started_at_ms    bigint NOT NULL,
    ended_at_ms      bigint,
    ended_by         text,
    node             text NOT NULL,
    kept_alive_at_ms bigint NOT NULL
);

-- The one query a backoffice makes: this room's meetings, newest first. The
-- id is in the key so that two meetings that started in the same millisecond
-- come back in a stable order rather than swapping places between pages.
CREATE INDEX IF NOT EXISTS mediacore_meeting_by_room
    ON mediacore_meeting (room, started_at_ms DESC, id DESC);

-- Partial, and small: it holds the meetings running right now across every
-- node, which is what the lease sweep reads every few seconds. A full index
-- on kept_alive_at_ms would carry every meeting ever held to answer a
-- question that is only ever about the handful still open.
CREATE INDEX IF NOT EXISTS mediacore_meeting_unfinished
    ON mediacore_meeting (kept_alive_at_ms)
    WHERE ended_at_ms IS NULL;

CREATE TABLE IF NOT EXISTS mediacore_attendance (
    meeting      text NOT NULL REFERENCES mediacore_meeting (id) ON DELETE CASCADE,
    ordinal      integer NOT NULL,
    sid          text NOT NULL,
    identity     text NOT NULL,
    display_name text NOT NULL,
    kind         text NOT NULL,
    account_id   text,
    joined_at_ms bigint NOT NULL,
    left_at_ms   bigint,
    PRIMARY KEY (meeting, ordinal)
);

-- Which meetings an account was at. Partial because most of the rows that
-- have no account are recorders and agents, and nobody asks for those by
-- account.
CREATE INDEX IF NOT EXISTS mediacore_attendance_by_account
    ON mediacore_attendance (account_id, meeting)
    WHERE account_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS mediacore_recording (
    id            text PRIMARY KEY,
    meeting       text NOT NULL REFERENCES mediacore_meeting (id) ON DELETE CASCADE,
    participant   text NOT NULL,
    account_id    text,
    segment       integer NOT NULL,
    local_path    text NOT NULL,
    object_key    text,
    bytes         bigint,
    sample_rate   integer NOT NULL,
    channels      smallint NOT NULL,
    started_at_ms bigint NOT NULL,
    ended_at_ms   bigint,
    offset_ms     bigint NOT NULL,
    duration_ms   bigint,
    -- One file per stretch of one participant's audio. The recorder writing
    -- the same segment down twice -- because it retried, or because it was
    -- restarted -- has to land on the row it already wrote and not beside it.
    UNIQUE (meeting, participant, segment)
);

CREATE INDEX IF NOT EXISTS mediacore_recording_by_meeting
    ON mediacore_recording (meeting);

CREATE TABLE IF NOT EXISTS mediacore_recording_work (
    recording        text NOT NULL REFERENCES mediacore_recording (id) ON DELETE CASCADE,
    stage            text NOT NULL,
    state            text NOT NULL,
    attempts         integer NOT NULL DEFAULT 0,
    due_at_ms        bigint NOT NULL,
    claimed_by       text,
    claim_ends_at_ms bigint,
    done_at_ms       bigint,
    last_error       text,
    PRIMARY KEY (recording, stage)
);

-- What a poller asks for: the work of one stage that is due. Two partial
-- indexes rather than one over every row, because a finished recording's two
-- rows stay in this table for ever as the record of when it went up and when
-- it was announced, and a queue whose index carries its own history gets
-- slower every day it runs.
CREATE INDEX IF NOT EXISTS mediacore_recording_work_waiting
    ON mediacore_recording_work (stage, due_at_ms)
    WHERE state = 'waiting';

CREATE INDEX IF NOT EXISTS mediacore_recording_work_claimed
    ON mediacore_recording_work (stage, claim_ends_at_ms)
    WHERE state = 'claimed';
",
    },
    Migration {
        version: 2,
        name: "which node's disk a recording is on",
        sql: "
-- Added rather than put into the first migration, because the first one may
-- already have run somewhere. Nullable and with no default, so the release
-- before this one goes on working against a database that has been upgraded:
-- it never mentions the column and Postgres never asks it to.
ALTER TABLE mediacore_recording ADD COLUMN IF NOT EXISTS node text;
",
    },
    Migration {
        version: 3,
        name: "finding the work nobody is going to move",
        sql: "
-- `/healthz` carries how much recording work is stranded -- given up on, or
-- overdue by so far that whoever it was waiting for is not coming. The waiting
-- half of that count already has an index; the failed half did not, and a
-- count over `state = 'failed'` without one walks a table that keeps two rows
-- per recording for the life of the deployment. Partial, so it carries the
-- rows that are stuck and not the history of every recording that went fine.
CREATE INDEX IF NOT EXISTS mediacore_recording_work_failed
    ON mediacore_recording_work (stage)
    WHERE state = 'failed';
",
    },
    Migration {
        version: 4,
        name: "the meeting table becomes the session table",
        sql: "
-- Renamed in place rather than copied into a new table beside the old one.
-- These rows are meetings people were really at, and a copy is a chance to end
-- up with some of them in one table and the rest in the other.
--
-- Every statement is written to do nothing when it has already been done, so
-- that the second node to start finds the work finished rather than an error.
-- Postgres has no IF NOT EXISTS on a rename, which is why this is a block that
-- looks before it moves rather than a list of ALTERs.
DO $$
BEGIN
    IF to_regclass('mediacore_meeting') IS NOT NULL
       AND to_regclass('mediacore_session') IS NULL THEN
        ALTER TABLE mediacore_meeting RENAME TO mediacore_session;
    END IF;

    IF EXISTS (SELECT FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = 'mediacore_attendance'
                  AND column_name = 'meeting') THEN
        ALTER TABLE mediacore_attendance RENAME COLUMN meeting TO session;
    END IF;

    IF EXISTS (SELECT FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = 'mediacore_recording'
                  AND column_name = 'meeting') THEN
        ALTER TABLE mediacore_recording RENAME COLUMN meeting TO session;
    END IF;

    -- The constraints Postgres named after the table and the column when
    -- migration 1 created them. Renaming a table does not touch them, so
    -- `\\d mediacore_session` would otherwise still say `meeting` in four
    -- places nobody thought to look.
    IF EXISTS (SELECT FROM pg_constraint WHERE conname = 'mediacore_meeting_pkey') THEN
        ALTER TABLE mediacore_session
            RENAME CONSTRAINT mediacore_meeting_pkey TO mediacore_session_pkey;
    END IF;

    IF EXISTS (SELECT FROM pg_constraint
                WHERE conname = 'mediacore_attendance_meeting_fkey') THEN
        ALTER TABLE mediacore_attendance
            RENAME CONSTRAINT mediacore_attendance_meeting_fkey
                           TO mediacore_attendance_session_fkey;
    END IF;

    IF EXISTS (SELECT FROM pg_constraint
                WHERE conname = 'mediacore_recording_meeting_fkey') THEN
        ALTER TABLE mediacore_recording
            RENAME CONSTRAINT mediacore_recording_meeting_fkey
                           TO mediacore_recording_session_fkey;
    END IF;

    IF EXISTS (SELECT FROM pg_constraint
                WHERE conname = 'mediacore_recording_meeting_participant_segment_key') THEN
        ALTER TABLE mediacore_recording
            RENAME CONSTRAINT mediacore_recording_meeting_participant_segment_key
                           TO mediacore_recording_session_participant_segment_key;
    END IF;
END
$$;

ALTER INDEX IF EXISTS mediacore_meeting_by_room RENAME TO mediacore_session_by_room;
ALTER INDEX IF EXISTS mediacore_meeting_unfinished RENAME TO mediacore_session_unfinished;
ALTER INDEX IF EXISTS mediacore_recording_by_meeting RENAME TO mediacore_recording_by_session;
",
    },
    Migration {
        version: 5,
        name: "somewhere for the platform above to keep its own data",
        sql: "
-- The room's metadata as it stood when the session opened, and each
-- participant's attributes as they stood when that person arrived. Opaque:
-- nothing in this server reads either of them, and nothing here has any
-- business knowing what an embedding platform decided to put in them.
--
-- text and not jsonb. Room metadata is a string that may or may not be JSON --
-- this server round-trips it either way -- and handing it to jsonb would mean
-- a platform whose metadata is not JSON could not have a record at all.
-- Attributes are a map this server serialises, and keeping them in the same
-- kind of column keeps one rule for both: what came in comes back out.
ALTER TABLE mediacore_session ADD COLUMN IF NOT EXISTS metadata text;
ALTER TABLE mediacore_attendance ADD COLUMN IF NOT EXISTS attributes text;
",
    },
    Migration {
        version: 6,
        name: "the account that was never ours to hold",
        sql: "
-- An account was one customer's idea of who a participant is, welded into a
-- schema every other platform embedding this server also has to live in. What
-- replaces it is the two columns migration 5 added: whoever wants to hang
-- their own identifier off a session now has a place to put it that this
-- server does not pretend to understand.
DROP INDEX IF EXISTS mediacore_attendance_by_account;

ALTER TABLE mediacore_attendance DROP COLUMN IF EXISTS account_id;
ALTER TABLE mediacore_recording DROP COLUMN IF EXISTS account_id;
",
    },
];

-- Last time a daemon reported it covers an agent suffix (`@host/folder`) in a channel.
create table coverage (
    channel_id uuid not null references channels(id) on delete cascade,
    suffix text not null,
    updated_at timestamptz not null default now(),
    primary key (channel_id, suffix)
);

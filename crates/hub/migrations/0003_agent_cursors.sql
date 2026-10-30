-- Highest message id each agent's client reports as delivered, per channel.
create table agent_cursors (
    channel_id uuid not null references channels(id) on delete cascade,
    agent_name text not null,
    last_read_id bigint not null,
    updated_at timestamptz not null default now(),
    primary key (channel_id, agent_name)
);

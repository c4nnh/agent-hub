CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE users (
    id uuid PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text NOT NULL,
    plan text NOT NULL DEFAULT 'free',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tokens (
    token_hash text PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE auth_requests (
    code text PRIMARY KEY,
    token text,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE channels (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    owner_id uuid NOT NULL REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_id, name)
);

CREATE TABLE channel_members (
    channel_id uuid NOT NULL REFERENCES channels(id),
    user_id uuid NOT NULL REFERENCES users(id),
    role text NOT NULL CHECK (role IN ('owner', 'member')),
    PRIMARY KEY (channel_id, user_id)
);

CREATE TABLE invitations (
    id uuid PRIMARY KEY,
    channel_id uuid NOT NULL REFERENCES channels(id),
    email text NOT NULL,
    invited_by uuid NOT NULL REFERENCES users(id),
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'accepted', 'rejected')),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE messages (
    id bigserial PRIMARY KEY,
    channel_id uuid NOT NULL REFERENCES channels(id),
    sender_user_id uuid NOT NULL REFERENCES users(id),
    sender_type text NOT NULL CHECK (sender_type IN ('human', 'agent')),
    sender_name text NOT NULL,
    body text NOT NULL,
    reply_to bigint,
    mentions text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE context_entries (
    id uuid PRIMARY KEY,
    channel_id uuid NOT NULL REFERENCES channels(id),
    title text NOT NULL,
    body text NOT NULL,
    version int NOT NULL DEFAULT 1,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    embedding vector(384)
);

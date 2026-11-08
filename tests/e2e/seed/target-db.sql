-- Seed the target-postgres (user data source) with a known typed schema.
-- Used by connector integration tests and e2e query-execution assertions.
-- Run against: postgres://target:target@localhost:5434/target_db

CREATE TABLE IF NOT EXISTS typed_table (
    id          SERIAL PRIMARY KEY,
    bool_col    BOOLEAN,
    int_col     INTEGER,
    bigint_col  BIGINT,
    float_col   DOUBLE PRECISION,
    numeric_col NUMERIC(12, 4),
    text_col    TEXT,
    date_col    DATE,
    ts_col      TIMESTAMP WITH TIME ZONE,
    json_col    JSONB,
    uuid_col    UUID,
    arr_col     INTEGER[]
);

-- Guarded so re-applying the seed never duplicates rows.
DO $$ BEGIN
IF NOT EXISTS (SELECT 1 FROM typed_table) THEN
INSERT INTO typed_table
    (bool_col, int_col, bigint_col, float_col, numeric_col, text_col, date_col, ts_col, json_col, uuid_col, arr_col)
VALUES
    (true,  42, 9000000000, 3.14, 1234.5678, 'hello', '2025-01-01', '2025-01-01T00:00:00Z', '{"key":"val"}', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '{1,2,3}'),
    (false, 0,  0,          0.0,  0.0000,    '',      '1970-01-01', '1970-01-01T00:00:00Z', 'null',          'b1ffcd00-0d1c-5f09-cc7e-7cc0ce491b22', '{}');
END IF;
END $$;

-- A realistic two-table schema for the "AI data-quality monitor suggestions" feature's backend
-- integration and live-provider tests (a nested-array structured-output shape needs real table/
-- column names to build a prompt against).
CREATE TABLE IF NOT EXISTS customers (
    id    BIGSERIAL PRIMARY KEY,
    email TEXT,
    phone TEXT
);

CREATE TABLE IF NOT EXISTS orders (
    id          BIGSERIAL PRIMARY KEY,
    created_at  TIMESTAMPTZ,
    customer_id BIGINT
);

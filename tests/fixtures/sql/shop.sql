-- Sample fixture database for `gap-ngc-sql` conformance and integration tests (`WP-M3-05`, `CMP-050`).

CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    tier TEXT NOT NULL,
    credit_cents INTEGER NOT NULL,
    region TEXT NOT NULL,
    active INTEGER NOT NULL,
    notes TEXT
);

INSERT INTO customers (id, name, tier, credit_cents, region, active, notes) VALUES
    (1,  'Ada Lovelace Analytics Labs',       'gold',     125000, 'eu', 1, 'founding enterprise partner'),
    (2,  'Alan Turing Computation Co',       'platinum', 240000, 'eu', 1, NULL),
    (3,  'Grace Hopper Systems Works',       'silver',    64000, 'us', 1, 'early compiler adopter'),
    (4,  'Edsger Dijkstra Graph Guild',      'gold',     152000, 'eu', 1, 'shortest path team'),
    (5,  'Barbara Liskov Abstractions',      'platinum', 310000, 'us', 1, NULL),
    (6,  'Donald Knuth Typesetting Press',   'gold',     175000, 'us', 1, 'literate engineering group'),
    (7,  'Margaret Hamilton Flight Systems', 'platinum', 420000, 'us', 1, 'fault-tolerant architecture'),
    (8,  'Leslie Lamport Consensus Lab',     'silver',    89000, 'us', 0, NULL),
    (9,  'Niklaus Wirth Modula Foundry',     'bronze',    32000, 'eu', 1, 'compact systems division'),
    (10, 'John Backus Functional Labs',      'silver',    95000, 'us', 1, NULL),
    (11, 'Frances Allen Optimizer Group',    'gold',     198000, 'us', 1, 'dataflow analysis unit'),
    (12, 'Tony Hoare Communicating Co',      'bronze',    48000, 'eu', 0, 'formal CSP verification');

CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    total_cents INTEGER NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL
);

INSERT INTO orders (id, customer_id, total_cents, status, created_at) VALUES
    (101, 1, 14900, 'paid',    '2025-02-01T10:00:00Z'),
    (102, 1,  8500, 'shipped', '2025-02-02T12:30:00Z'),
    (103, 2, 42000, 'paid',    '2025-02-03T08:15:00Z'),
    (104, 3,  6200, 'pending', '2025-02-04T16:45:00Z'),
    (105, 5, 88000, 'paid',    '2025-02-05T09:05:00Z'),
    (106, 6, 19500, 'shipped', '2025-02-06T14:20:00Z');

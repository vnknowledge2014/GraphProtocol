CREATE TABLE warehouses (
    id INTEGER PRIMARY KEY,
    code TEXT NOT NULL UNIQUE,
    region TEXT NOT NULL
);

CREATE TABLE shipments (
    id INTEGER PRIMARY KEY,
    warehouse_id INTEGER NOT NULL REFERENCES warehouses(id),
    tracking_no TEXT NOT NULL UNIQUE
);

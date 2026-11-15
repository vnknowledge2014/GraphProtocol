CREATE TABLE lines (id INT PRIMARY KEY, order_id TEXT REFERENCES orders(id), qty INT);
CREATE INDEX idx_lines_order ON lines(order_id);

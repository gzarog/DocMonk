-- Speeds up the nightly settlement report.
CREATE INDEX idx_orders_settled_at ON orders (settled_at)
    WHERE status = 'settled';

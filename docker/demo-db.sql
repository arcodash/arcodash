-- Sample e-commerce and analytics data for the demo-db service (docker compose --profile demo).
-- ── Schema ────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS departments (
  id         SERIAL PRIMARY KEY,
  name       TEXT NOT NULL,
  budget     NUMERIC(12,2) NOT NULL
);

CREATE TABLE IF NOT EXISTS employees (
  id            SERIAL PRIMARY KEY,
  department_id INT REFERENCES departments(id),
  name          TEXT NOT NULL,
  email         TEXT NOT NULL UNIQUE,
  title         TEXT NOT NULL,
  salary        NUMERIC(10,2) NOT NULL,
  hired_at      DATE NOT NULL,
  is_active     BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE IF NOT EXISTS categories (
  id   SERIAL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS products (
  id          SERIAL PRIMARY KEY,
  category_id INT REFERENCES categories(id),
  name        TEXT NOT NULL,
  sku         TEXT NOT NULL UNIQUE,
  price       NUMERIC(10,2) NOT NULL,
  cost        NUMERIC(10,2) NOT NULL,
  stock       INT NOT NULL DEFAULT 0,
  is_active   BOOLEAN NOT NULL DEFAULT true,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS customers (
  id            SERIAL PRIMARY KEY,
  name          TEXT NOT NULL,
  email         TEXT NOT NULL UNIQUE,
  country       TEXT NOT NULL,
  city          TEXT NOT NULL,
  plan          TEXT NOT NULL DEFAULT 'free',  -- free | starter | pro | enterprise
  signed_up_at  TIMESTAMPTZ NOT NULL,
  is_active     BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE IF NOT EXISTS orders (
  id             SERIAL PRIMARY KEY,
  customer_id    INT REFERENCES customers(id),
  employee_id    INT REFERENCES employees(id),
  status         TEXT NOT NULL,   -- pending | processing | shipped | delivered | cancelled | refunded
  total          NUMERIC(12,2) NOT NULL,
  discount       NUMERIC(5,2) NOT NULL DEFAULT 0,
  created_at     TIMESTAMPTZ NOT NULL,
  shipped_at     TIMESTAMPTZ,
  delivered_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS order_items (
  id          SERIAL PRIMARY KEY,
  order_id    INT REFERENCES orders(id),
  product_id  INT REFERENCES products(id),
  quantity    INT NOT NULL,
  unit_price  NUMERIC(10,2) NOT NULL,
  discount    NUMERIC(5,2) NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS page_events (
  id          BIGSERIAL PRIMARY KEY,
  customer_id INT REFERENCES customers(id),
  session_id  TEXT NOT NULL,
  page        TEXT NOT NULL,
  event       TEXT NOT NULL,   -- view | click | signup | purchase | churn
  country     TEXT NOT NULL,
  device      TEXT NOT NULL,   -- desktop | mobile | tablet
  occurred_at TIMESTAMPTZ NOT NULL
);

-- ── Seed data ─────────────────────────────────────────────────────────────────

INSERT INTO departments (name, budget) VALUES
  ('Engineering',      4200000),
  ('Sales',            1800000),
  ('Marketing',         950000),
  ('Customer Success',  620000),
  ('Product',          1100000),
  ('Operations',        480000),
  ('Finance',           390000),
  ('HR',                310000)
ON CONFLICT DO NOTHING;

-- 30 employees spread across departments, hired over the last 4 years
INSERT INTO employees (department_id, name, email, title, salary, hired_at, is_active)
SELECT
  1 + (i % 8),
  initcap(first_name || ' ' || last_name),
  lower(first_name || '.' || last_name || '@example.com'),
  title,
  ROUND((55000 + random() * 120000)::numeric, 2),
  (NOW() - (random() * interval '4 years'))::date,
  i <> 28
FROM (
  VALUES
    ('Alice','Chen','Senior Engineer'),('Bob','Martinez','VP Sales'),
    ('Carol','Smith','Growth Marketer'),('David','Johnson','CS Lead'),
    ('Emma','Brown','Product Manager'),('Frank','Davis','DevOps Engineer'),
    ('Grace','Wilson','Account Executive'),('Henry','Moore','Full-Stack Engineer'),
    ('Iris','Taylor','SDR'),('Jake','Anderson','Director of Marketing'),
    ('Kate','Thomas','CSM'),('Leo','Jackson','Backend Engineer'),
    ('Mia','White','Sales Manager'),('Noah','Harris','Analyst'),
    ('Olivia','Martin','UX Designer'),('Peter','Thompson','Frontend Engineer'),
    ('Quinn','Garcia','CS Director'),('Rachel','Martinez','Product Designer'),
    ('Sam','Robinson','Finance Manager'),('Tina','Clark','Recruiter'),
    ('Uma','Rodriguez','Senior Engineer'),('Victor','Lewis','Account Manager'),
    ('Wendy','Lee','Data Engineer'),('Xavier','Walker','CMO'),
    ('Yara','Hall','Content Marketer'),('Zane','Allen','Engineering Manager'),
    ('Amy','Young','Support Engineer'),('Ben','King','Solutions Engineer'),
    ('Clara','Wright','Operations Lead'),('Dan','Scott','CFO')
) AS t(first_name, last_name, title)
  CROSS JOIN generate_series(1,1) AS g(i)
ON CONFLICT (email) DO NOTHING;

INSERT INTO categories (name) VALUES
  ('Electronics'),('Apparel'),('Home & Garden'),('Sports & Outdoors'),
  ('Books'),('Toys & Games'),('Beauty'),('Food & Drink'),
  ('Office'),('Pet Supplies')
ON CONFLICT (name) DO NOTHING;

-- 150 products, realistic SKU + pricing per category
INSERT INTO products (category_id, name, sku, price, cost, stock, is_active, created_at)
SELECT
  cat_id,
  name,
  'SKU-' || LPAD((ROW_NUMBER() OVER ())::text, 5, '0'),
  ROUND((base_price * (0.8 + random() * 0.4))::numeric, 2),
  ROUND((base_price * (0.3 + random() * 0.2))::numeric, 2),
  (50 + (random() * 450))::int,
  random() > 0.08,
  NOW() - (random() * interval '3 years')
FROM (VALUES
  (1,'Wireless Headphones',120),(1,'Mechanical Keyboard',95),(1,'USB-C Hub',45),
  (1,'4K Webcam',85),(1,'Portable SSD 1TB',110),(1,'Smart Speaker',70),
  (1,'Ergonomic Mouse',55),(1,'LED Desk Lamp',35),(1,'Phone Stand',18),
  (1,'Bluetooth Earbuds',65),(1,'Monitor Arm',75),(1,'Cable Management Kit',22),
  (1,'Power Bank 20000mAh',40),(1,'Smart Plug (4-pack)',30),(1,'Laptop Stand',50),
  (2,'Classic Tee',28),(2,'Slim Chinos',65),(2,'Running Shorts',38),
  (2,'Wool Sweater',85),(2,'Rain Jacket',120),(2,'Canvas Sneakers',72),
  (2,'Merino Socks (6-pack)',30),(2,'Denim Jacket',110),(2,'Fleece Hoodie',68),
  (2,'Cotton Polo',42),(2,'Compression Leggings',55),(2,'Baseball Cap',25),
  (3,'Bamboo Cutting Board',35),(3,'Cast Iron Skillet',60),(3,'Herb Garden Kit',28),
  (3,'Throw Blanket',55),(3,'Scented Candle Set',32),(3,'Wall Clock',48),
  (3,'Ceramic Mug Set',38),(3,'Picture Frame Set',22),(3,'Indoor Plant Pot',20),
  (3,'Sheet Set Queen',80),(3,'Comforter King',110),(3,'Kitchen Timer',15),
  (4,'Yoga Mat',40),(4,'Resistance Bands',22),(4,'Foam Roller',30),
  (4,'Jump Rope',15),(4,'Water Bottle 32oz',28),(4,'Hiking Backpack',95),
  (4,'Camping Lantern',45),(4,'Trekking Poles',65),(4,'Bike Helmet',75),
  (4,'Running Shoes',110),(4,'Gym Gloves',20),(4,'Protein Shaker',18),
  (5,'Clean Code',42),(5,'The Pragmatic Programmer',48),(5,'Atomic Habits',22),
  (5,'Deep Work',18),(5,'Zero to One',20),(5,'The Lean Startup',22),
  (5,'Designing Data-Intensive Applications',58),(5,'Building Microservices',52),
  (5,'Python Crash Course',38),(5,'SQL for Data Analysis',34),
  (6,'Building Block Set 500pc',55),(6,'Strategy Board Game',45),
  (6,'Remote Control Car',65),(6,'Puzzle 1000pc',28),(6,'Art Set 120pc',38),
  (6,'Science Kit',48),(6,'Card Game Bundle',22),(6,'Wooden Chess Set',75),
  (7,'Face Serum',55),(7,'Vitamin C Moisturiser',42),(7,'Retinol Eye Cream',48),
  (7,'Hydrating Toner',32),(7,'Sunscreen SPF50',28),(7,'Lip Balm Set',18),
  (7,'Hair Mask',22),(7,'Dry Shampoo',16),(7,'Body Lotion',24),
  (8,'Specialty Coffee 250g',18),(8,'Matcha Powder',28),(8,'Protein Bar Box',45),
  (8,'Herbal Tea Variety',22),(8,'Dark Chocolate Box',24),(8,'Cold Brew Kit',35),
  (8,'Kombucha Starter',32),(8,'Spice Collection',38),(8,'Olive Oil Extra Virgin',30),
  (9,'Wireless Charger Pad',32),(9,'Standing Desk Mat',55),(9,'Monitor Riser',42),
  (9,'Desk Organiser',28),(9,'Whiteboard Markers',14),(9,'Label Maker',45),
  (9,'Document Scanner',110),(9,'Shredder',85),(9,'Ergonomic Chair Cushion',48),
  (10,'Dog Harness',38),(10,'Cat Toy Set',18),(10,'Pet Water Fountain',55),
  (10,'Dog Bed Large',75),(10,'Cat Tree',90),(10,'Bird Feeder',25),
  (10,'Fish Tank 10gal',65),(10,'Pet Grooming Kit',32),(10,'Automatic Feeder',60),
  -- more electronics and apparel to hit 150
  (1,'VR Headset',320),(1,'Gaming Controller',65),(1,'Noise Cancelling Headset',180),
  (1,'Smart Watch',220),(1,'Action Camera',195),(1,'Ring Light Kit',55),
  (2,'Performance T-Shirt',35),(2,'Windbreaker',90),(2,'Cargo Pants',75),
  (2,'Thermal Underlayer',50),(2,'Swim Shorts',40),(2,'Sports Bra',45),
  (3,'Air Purifier',145),(3,'Humidifier',65),(3,'Electric Kettle',45),
  (3,'Blender',80),(3,'Coffee Grinder',55),(3,'Bread Bin',35),
  (4,'Kettlebell 20kg',55),(4,'Pull-up Bar',38),(4,'Ab Roller',22),
  (4,'Agility Ladder',28),(4,'Slackline Kit',48),(4,'Rock Climbing Shoes',95),
  (5,'System Design Interview',52),(5,'Cracking the Coding Interview',45),
  (5,'Machine Learning Yearning',0),(5,'The Staff Engineer Path',38),
  (6,'Telescope Beginner',120),(6,'Drone Kit',185),(6,'Watercolor Set',42),
  (7,'Perfume Gift Set',75),(7,'Essential Oil Diffuser',48),(7,'Jade Roller',25),
  (8,'Smoothie Maker',65),(8,'Pour-Over Coffee Set',48),(8,'Nut Butter 3-pack',28),
  (9,'USB Hub 10-port',55),(9,'Cable Ties Assorted',12),(9,'Laptop Lock',28),
  (10,'Training Clicker',8),(10,'Pet First Aid Kit',32),(10,'Reptile Heat Mat',28)
) AS t(cat_id, name, base_price)
ON CONFLICT (sku) DO NOTHING;

-- 1000 customers across 25 countries, signed up over the last 3 years
INSERT INTO customers (name, email, country, city, plan, signed_up_at, is_active)
SELECT
  initcap(first_name || ' ' || last_name),
  lower(first_name || '.' || last_name || i || '@' || domain),
  country,
  city,
  plan,
  NOW() - (random() * interval '3 years'),
  random() > 0.05
FROM (
  SELECT
    i,
    (ARRAY['James','Emma','Liam','Olivia','Noah','Ava','William','Sophia','Benjamin','Isabella',
           'Lucas','Mia','Mason','Charlotte','Ethan','Amelia','Alexander','Harper','Henry','Evelyn',
           'Sebastian','Abigail','Jack','Emily','Michael','Elizabeth','Owen','Sofia','Daniel','Ella',
           'Logan','Scarlett','Samuel','Victoria','David','Chloe','Joseph','Riley','Carter','Aria',
           'Wyatt','Lily','John','Zoey','Luke','Penelope','Gabriel','Nora','Anthony','Layla',
           'Julian','Hannah','Lincoln','Lillian','Jaxon','Addison','Isaac','Aubrey','Joshua','Ellie']
    )[(i % 60) + 1] AS first_name,
    (ARRAY['Smith','Johnson','Williams','Brown','Jones','Garcia','Miller','Davis','Wilson','Moore',
           'Taylor','Anderson','Thomas','Jackson','White','Harris','Martin','Thompson','Young','King',
           'Lee','Roberts','Turner','Evans','Walker','Hall','Lewis','Allen','Hill','Scott',
           'Green','Baker','Adams','Nelson','Carter','Mitchell','Perez','Roberts','Campbell','Parker']
    )[(i % 40) + 1] AS last_name,
    (ARRAY['example.com','example.org','example.net'])[(i % 3) + 1] AS domain,
    (ARRAY[
      'United States','United States','United States','United States','United States',
      'United Kingdom','United Kingdom','Canada','Canada','Germany',
      'Germany','France','France','Australia','Australia',
      'Netherlands','Spain','Japan','Japan','Brazil',
      'India','India','Singapore','Sweden','Norway',
      'Italy','Portugal','Poland','Mexico','South Korea'
    ])[(i % 30) + 1] AS country,
    (ARRAY[
      'New York','Los Angeles','Chicago','Houston','Phoenix',
      'London','Manchester','Toronto','Vancouver','Berlin',
      'Munich','Paris','Lyon','Sydney','Melbourne',
      'Amsterdam','Madrid','Tokyo','Osaka','São Paulo',
      'Mumbai','Bangalore','Singapore','Stockholm','Oslo',
      'Rome','Lisbon','Warsaw','Mexico City','Seoul'
    ])[(i % 30) + 1] AS city,
    (ARRAY['free','free','free','starter','starter','pro','pro','enterprise'])
    [(i % 8) + 1] AS plan
  FROM generate_series(1, 1000) AS g(i)
) t
ON CONFLICT (email) DO NOTHING;

-- 5000 orders over the last 2 years with realistic patterns
-- More orders in Q4, weekdays heavier, exponential growth trend
INSERT INTO orders (customer_id, employee_id, status, total, discount, created_at, shipped_at, delivered_at)
SELECT
  1 + (random() * 999)::int,
  1 + (random() * 29)::int,
  status,
  ROUND((25 + random() * 475)::numeric, 2),
  ROUND((CASE WHEN random() < 0.3 THEN random() * 20 ELSE 0 END)::numeric, 2),
  created_at,
  CASE WHEN status NOT IN ('pending','cancelled') THEN created_at + interval '1 day' + (random() * interval '3 days') END,
  CASE WHEN status IN ('delivered') THEN created_at + interval '4 days' + (random() * interval '5 days') END
FROM (
  SELECT
    ts AS created_at,
    (ARRAY['pending','processing','processing','shipped','shipped','delivered',
           'delivered','delivered','delivered','cancelled','refunded'])
    [(random() * 10 + 1)::int] AS status
  FROM (
    SELECT NOW() - ((2.0 * random()^0.6) * interval '730 days') AS ts
    FROM generate_series(1, 5000)
  ) t
) t2;

-- Order items: 2-4 items per order (LATERAL picks a valid random product)
INSERT INTO order_items (order_id, product_id, quantity, unit_price, discount)
SELECT
  o.id,
  p.id,
  1 + (random() * 4)::int,
  p.price,
  ROUND((CASE WHEN random() < 0.2 THEN random() * 15 ELSE 0 END)::numeric, 2)
FROM orders o
CROSS JOIN generate_series(1, 2 + (random() * 2)::int) AS g(n)
CROSS JOIN LATERAL (SELECT id, price FROM products ORDER BY random() LIMIT 1) p;

-- Sync order totals to actual line-item sums
UPDATE orders o
SET total = sub.line_total
FROM (
  SELECT order_id, ROUND(SUM(quantity * unit_price * (1 - discount/100))::numeric, 2) AS line_total
  FROM order_items
  GROUP BY order_id
) sub
WHERE o.id = sub.order_id AND sub.line_total > 0;

-- 20000 page events over the last 6 months
INSERT INTO page_events (customer_id, session_id, page, event, country, device, occurred_at)
SELECT
  CASE WHEN random() < 0.7 THEN (1 + (random() * 999)::int) ELSE NULL END,
  md5(random()::text),
  page,
  event,
  (ARRAY['United States','United States','United States','United Kingdom','Canada',
         'Germany','France','Australia','Japan','Brazil','India','Singapore',
         'Sweden','Netherlands','Spain','Italy','Mexico','South Korea','Norway','Poland']
  )[(random() * 19 + 1)::int],
  (ARRAY['desktop','desktop','desktop','mobile','mobile','tablet'])[(random() * 5 + 1)::int],
  NOW() - (random()^0.5 * interval '180 days')
FROM (
  SELECT
    (ARRAY['/','/','/pricing','/pricing','/features','/docs','/blog',
           '/signup','/login','/dashboard','/queries','/dashboards','/alerts'])
    [(random() * 12 + 1)::int] AS page,
    (ARRAY['view','view','view','click','click','signup','purchase','churn'])
    [(random() * 7 + 1)::int] AS event
  FROM generate_series(1, 20000)
) t;

-- ── Indexes for fast queries ──────────────────────────────────────────────────
CREATE INDEX IF NOT EXISTS idx_orders_created_at  ON orders(created_at);
CREATE INDEX IF NOT EXISTS idx_orders_status       ON orders(status);
CREATE INDEX IF NOT EXISTS idx_orders_customer     ON orders(customer_id);
CREATE INDEX IF NOT EXISTS idx_order_items_order   ON order_items(order_id);
CREATE INDEX IF NOT EXISTS idx_order_items_product ON order_items(product_id);
CREATE INDEX IF NOT EXISTS idx_events_occurred_at  ON page_events(occurred_at);
CREATE INDEX IF NOT EXISTS idx_events_page         ON page_events(page);
CREATE INDEX IF NOT EXISTS idx_customers_country   ON customers(country);
CREATE INDEX IF NOT EXISTS idx_customers_plan      ON customers(plan);

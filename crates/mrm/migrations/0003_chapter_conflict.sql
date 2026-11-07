-- Chapter-number collisions (M10 guard rails). Additive and nullable: binaries
-- built before this migration never read or write it and keep working.
ALTER TABLE manhwa ADD COLUMN chapter_conflict TEXT;

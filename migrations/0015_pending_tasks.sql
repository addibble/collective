-- Newcomers: pending tasks and vouching (REVIEWS.md). A real patch's task
-- opens only when its author has standing or an operator with standing
-- vouches for it; until then it waits as `pending`.
--
-- shards.state is free text. Its values are now:
--   pending     waiting for the author's standing or a vouch; not offered
--   open        offered to reviewers
--   challenged  reviewers agreed on a finding; a challenge is open
--   clean       the full panel found nothing
--   graded      the challenge the reviewers raised was resolved
--   verifying   a contributed canary awaiting runners (contributions)
--   retired     a contributed canary taken out of the queue
-- Canary tasks are never pending.

-- The operator that vouched for the task, and when; its loan of rep
-- (`vouch_loan` in the ledger) is out until the task resolves.
ALTER TABLE shards ADD COLUMN vouched_by TEXT;
ALTER TABLE shards ADD COLUMN vouched_at INTEGER;
-- How the loan settled, claimed once: 'returned' (`vouch_returned` in the
-- ledger) or 'lost' (the debit stands). NULL while it is out, or unvouched.
ALTER TABLE shards ADD COLUMN vouch_settled TEXT;

-- The sweep that opens pending tasks of authors who now have standing walks
-- pending shards by author.
CREATE INDEX shards_pending_author ON shards(state, author_op);
-- Trust sums an operator's authored patch scores; without this every trust
-- lookup scans the patches table.
CREATE INDEX patches_author ON patches(author_op);

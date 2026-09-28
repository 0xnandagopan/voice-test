-- Delivery survives changes to the currently offered question and control retries.
ALTER TABLE question_permits ADD COLUMN delivered_at timestamptz;
UPDATE question_permits q SET delivered_at=clock_timestamp()
WHERE EXISTS(SELECT 1 FROM voice_hook_bindings b WHERE b.delivered_permit_id=q.id)
   OR EXISTS(SELECT 1 FROM voice_answer_permits a WHERE a.question_permit_id=q.id);

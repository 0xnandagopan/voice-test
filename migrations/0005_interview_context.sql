-- Operator background guides questions; it is never customer testimony.
ALTER TABLE interviews ADD COLUMN context_attachments jsonb NOT NULL DEFAULT '[]';
ALTER TABLE interviews ADD COLUMN context_hash text NOT NULL DEFAULT '';
ALTER TABLE interviews ADD COLUMN interview_questions jsonb;
ALTER TABLE interviews ADD COLUMN interview_preparation text NOT NULL DEFAULT 'not_required'
    CHECK (interview_preparation IN ('not_required','queued','running','ready','failed'));
ALTER TABLE interviews ADD CONSTRAINT bounded_context_attachments CHECK (
    jsonb_typeof(context_attachments)='array' AND jsonb_array_length(context_attachments)<=5
);

ALTER TABLE `sessions` ADD `assistant_id` text;--> statement-breakpoint
UPDATE `sessions`
SET `assistant_id` = (
    SELECT CASE WHEN json_valid(`data`) THEN json_extract(`data`, '$.assistant.id') END
    FROM `session_details`
    WHERE `session_details`.`session_id` = `sessions`.`id`
)
WHERE `provider` = 'assistant';

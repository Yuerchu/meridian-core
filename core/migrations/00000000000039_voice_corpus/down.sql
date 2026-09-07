-- 行没了，磁盘上的 `voice_corpus/` 目录还在。回退一个迁移不该销毁声纹语料
-- ——那是一个显式的删除动作，有它自己的命令和确认。留下的目录会被重新应用
-- 这个迁移之后的恢复器当作孤儿文件清理，或者由人自己处理。
DROP TABLE voice_sender_optouts;
DROP TABLE voice_clips;
DROP TABLE voice_blobs;

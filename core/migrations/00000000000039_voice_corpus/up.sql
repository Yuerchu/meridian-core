-- 群友发来的语音，连同它的转写，按会话白名单留存。
--
-- 存的是真人声纹，所以默认全关：一个 (bot 账号, 会话) 要出现在
-- `onebot.voice_capture_sessions` 里，它的语音才会落盘。白名单是许可而不是
-- 过滤器——没在名单上的会话，这里不该有它的行，一行都不该有。
--
-- 为什么是两张表而不是一张：同一段音频在群里被两个人发出来（转发、复读）
-- 是**两次采集**——两个发送者、两条消息、两个时间。按 (会话, sha) 单表唯一
-- 会把它们合并成一行，只留下第一个人，于是"按人删除"失灵，训练标签也是错的。
-- 物理文件按内容去重，采集事件按出现去重，两者是不同的东西。
--
-- 为什么不复活 `attachments`：它按 message_id 挂，而被引用消息里的语音根本
-- 没有自己的 messages 行；它也没有转写、没有转写来源、没有发布状态。
--
-- 为什么不塞 messages.content 的 parts JSON：那个数组是**要发给 provider 的
-- 东西**，往里塞非标准 part 最好的情况是被忽略。而且"导出全部语料"会变成扫
-- 全表解 JSON，"按会话删除"会变成重写消息体。

-- 物理文件。
CREATE TABLE voice_blobs (
  id TEXT PRIMARY KEY NOT NULL,

  -- 授权是给"这个 bot 账号在这个会话里"的，不是给"这个会话"的：两个 bot
  -- 各自被拉进同一个群，是两次独立的同意。所以账号是主键的一部分，不是
  -- 附注。少了它，两个账号同群、同 hash 会指向同一个物理文件。
  bot_self_id BIGINT NOT NULL,
  source_type TEXT NOT NULL,          -- onebot_group | onebot_private
  source_id TEXT NOT NULL,

  sha256 TEXT NOT NULL,

  -- 由字节 magic 判定的受限枚举，**不信** URL、文件名或 Content-Type——
  -- 三者都由对端控制，而这个值决定文件落进哪个去重桶。
  file_format TEXT NOT NULL,          -- silk | amr | mp3 | wav | ogg
  -- <sha256>.<ext>。内容寻址：并发两个任务写同一路径、同样的字节。
  file_name TEXT NOT NULL,
  file_size BIGINT NOT NULL,

  -- pending | ready | damaged | deleting
  --
  --   pending  有 owner 正在发布这个文件，或者那个 owner 已经死了
  --            （靠 lease 与 fencing 分辨，见下）。
  --   ready    可列出、可导出。
  --   damaged  文件缺失，或 size/sha 校验不过。**必须是独立状态**：留在
  --            ready 会被导出，改回 pending 又没有 owner，而且它可能已经
  --            有 clips 挂着。
  --   deleting 墓碑。文件删成功之后才删这一行——先删行会让路径丢失，而
  --            Windows 上文件被占用导致删除失败是常事，那时语料就再也找
  --            不到了。恢复器负责重试。
  status TEXT NOT NULL DEFAULT 'pending',

  -- 谁在发布它。**每次 claim 生成一个新的 token**，`fence_epoch` 单调递增。
  --
  -- 为什么两个都要、为什么不能只记进程 id：同一个进程里 Task A 持有 pending、
  -- lease 过期、Task B 执行 `CAS WHERE owner=旧值` 时写回的是**相同的值**，
  -- 于是两个任务都认为自己是 owner，都去发布。token 让每次 claim 可区分，
  -- epoch 让接管可排序——旧 owner 醒来时带的是旧 epoch，条件不匹配，写不进去。
  --
  -- publish 之前和最终事务里都必须带
  -- `WHERE status='pending' AND owner_token=? AND fence_epoch=?`；
  -- affected 为 0 就是丢了所有权，那个任务只能重读这一行、等它变 ready。
  owner_token TEXT,
  fence_epoch BIGINT NOT NULL DEFAULT 0,
  lease_expires_at BIGINT,

  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,

  CHECK (status IN ('pending', 'ready', 'damaged', 'deleting')),
  -- 只有 pending 有 owner。别的状态带着 owner 是一个没人会去查、却会让恢复器
  -- 误判成"有人正在写"的组合。
  CHECK (
    (status =  'pending' AND owner_token IS NOT NULL AND lease_expires_at IS NOT NULL) OR
    (status <> 'pending' AND owner_token IS     NULL AND lease_expires_at IS     NULL)
  )
);

-- 去重按 (账号, 会话) 而不是全局：同一段音频出现在两个群里是两次独立的同意，
-- 全局唯一会让第一个群的删除顺手抹掉第二个群的语料。代价是磁盘上多一份拷贝，
-- 而那正是"删一个不影响另一个"的实现方式。
CREATE UNIQUE INDEX idx_voice_blobs_dedupe
  ON voice_blobs(bot_self_id, source_type, source_id, file_format, sha256);

-- 恢复器问的那个问题：有没有过期的 pending、有没有待重试的墓碑。
CREATE INDEX idx_voice_blobs_status ON voice_blobs(status, lease_expires_at);

-- 一次采集事件。
CREATE TABLE voice_clips (
  id TEXT PRIMARY KEY NOT NULL,
  blob_id TEXT NOT NULL REFERENCES voice_blobs(id) ON DELETE CASCADE,

  -- 与 blob 冗余，因为下面那个 occurrence 唯一索引和按会话查询都要用。
  -- **写入时从 blob 行读取，不从调用参数传**，否则两边可能不一致。
  bot_self_id BIGINT NOT NULL,
  source_type TEXT NOT NULL,
  source_id TEXT NOT NULL,

  -- 消息的**发送者**，不是声学意义上的说话人：转发别人的语音时两者不同。
  -- 名字如实写，否则 UI 会声称"删除此人的全部声音"，而那不是它能兑现的。
  sender_id TEXT NOT NULL,

  platform_message_id BIGINT,
  -- 一条消息理论上可以带多个 record 段。少了这一维，第二段会静默消失。
  segment_index INTEGER NOT NULL DEFAULT 0,

  -- NULL 表示转写没拿到（适配器超时、返回空），或者这条消息有多个 record 段
  -- ——`voice_msg_to_text` 按**消息**作答，一段覆盖两条的转写不能归给其中
  -- 任何一条。行照留：音频本身是语料，只是导出默认跳过没有转写的那些。
  transcript TEXT,
  transcript_source TEXT,

  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,

  -- 两者同生同灭。有文本没来源的行没法判断能不能用来训练。
  CHECK (
    (transcript IS NULL AND transcript_source IS NULL) OR
    (transcript IS NOT NULL AND transcript_source IS NOT NULL)
  )
);

-- 键里带账号和会话：OneBot 只把 message_id 定义为整数消息 ID，没有跨会话或
-- 跨账号唯一的承诺。同一个事件重投时靠它保持幂等。
CREATE UNIQUE INDEX idx_voice_clips_occurrence
  ON voice_clips(bot_self_id, source_type, source_id, platform_message_id, segment_index)
  WHERE platform_message_id IS NOT NULL;

CREATE INDEX idx_voice_clips_blob ON voice_clips(blob_id);

-- "把我的声音删掉"跨会话，所以它需要自己的索引。
CREATE INDEX idx_voice_clips_sender ON voice_clips(sender_id);

-- "以后别再录我"。
--
-- 与删除历史是两件不同的事：删除处理已有数据，这个拒绝的是未来。借 session
-- 白名单表达会把整个群停掉，而那不是这个人要求的。
--
-- scope 是**全局 sender**：一个人说了不录，不该要求他对每个群、每个 bot 账号
-- 再分别说一次。
CREATE TABLE voice_sender_optouts (
  sender_id TEXT PRIMARY KEY NOT NULL,
  created_at BIGINT NOT NULL
);

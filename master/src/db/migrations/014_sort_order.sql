-- 列表顺序(DESIGN.md §10):订阅里节点的先后 = 机器的顺序 × 机器内节点的顺序。
--
-- 两列都只影响订阅与界面里的排列,**不进 sing-box 配置**:`build_agent_config`
-- 的 inbounds 照旧按 id 排,所以挪一下顺序不推进任何 revision、不重建 box。
--
-- 回填成 id:升级前两处列表都是按 id 排的(nodes 再按 agent_id 分组),
-- 于是升级前后看到的顺序一个字不变。新行由 INSERT 自己取 max+1 落到末尾。
ALTER TABLE nodes  ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
UPDATE nodes  SET sort_order = id;
UPDATE agents SET sort_order = id;

## Problem

当前持久化层基于 JSON 文件：每次保存全量重写所有消息（写放大）、写入非原子（崩溃可能损坏）、session 列表需扫目录逐个读文件、rewind/fork 需要读整个文件再改写。

需要迁移到 PostgreSQL，为后续功能（rewind、fork、懒加载）提供关系型查询和事务支持。

## Scenarios

1. **Rewind 截断：** 按条件删除消息，不需要读写整个历史。

2. **Session 列表：** 查询替代目录扫描。

3. **崩溃安全：** 写入在事务中完成，要么全部成功要么全部回滚。

## Scope

**In:**
- 引入 PostgreSQL 依赖
- 消息按行存储（非整包序列化），支持行级增量写入
- 实现 PG 版存储（替代 `JsonFileStorage` 和文件版 `WorkspaceStorage`）
- `coda-server.toml` 增加数据库连接配置

**Out:**
- 不做旧数据迁移工具（breaking change acceptable）
- 不支持 SQLite 作为备选后端
- 不做分页查询和懒加载（P2）

## Constraints

- 部署环境需要可用的 PostgreSQL 实例（Docker 运行，数据 mount 在宿主机）。
- 热路径（streaming events、内存中的 snapshot）不走 DB，DB 只用于持久化和恢复。
- 当前 `SessionStorage` trait 以整包 checkpoint 为读写单元，迁移时需要调整接口以匹配行级存储。

## Success Criteria

- Session 的所有持久化数据存储在 PostgreSQL 中，不再使用 JSON 文件。
- 消息行级存储，保存时只写入新增消息。
- 写入有事务保障。
- 现有功能行为不变。

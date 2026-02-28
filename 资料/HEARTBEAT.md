# HEARTBEAT.md — 犇犇娃的定期任务

## RSS 源清单

### 综合/澳洲
- 澳洲新闻: https://www.abc.net.au/news/feed/51120/rss.xml
- SBS 中文: https://www.sbs.com.au/chinese/feed

### 科技/AI/机器人
- TechCrunch AI: https://techcrunch.com/category/artificial-intelligence/feed/
- HackerNews AI: https://hnrss.org/newest?q=AI
- HackerNews 机器人: https://hnrss.org/newest?q=robotics
- HackerNews 无人机: https://hnrss.org/newest?q=drone
- Reddit AI: https://www.reddit.com/r/artificial/.rss
- Reddit 机器人: https://www.reddit.com/r/robotics/.rss

### 无人机/军事/采购
- Reddit 无人机: https://www.reddit.com/r/drones/.rss
- Reddit 多旋翼: https://www.reddit.com/r/Multicopter/.rss
- Breaking Defense: https://breakingdefense.com/feed/
- Defense News: https://www.defensenews.com/arc/outboundfeeds/rss/
- Defense Industry Daily: https://www.defenseindustrydaily.com/feed/
- DSCA 军售通报: https://www.dsca.mil/RSS (美国对外军售官方公告)
- Janes 防务情报: https://www.janes.com/feeds/news

### 中国/政治/科技圈
- Reddit 中国政治: https://www.reddit.com/r/chinesepolitics/.rss
- Reddit 中国军事: https://www.reddit.com/r/Sino/.rss
- Reddit 中国讨论: https://www.reddit.com/r/China/.rss
- 36氪: https://36kr.com/feed
- 科技新闻: https://www.cnbeta.com.tw/rss.xml
- Linux.do 开发板: https://linux.do/c/develop/4.rss
- V2EX 热帖: https://www.v2ex.com/index.xml

### 业余无线电/Maker
- Reddit 业余无线电: https://www.reddit.com/r/amateurradio/.rss
- HackerNews 首页: https://hnrss.org/frontpage

### 金融/跨境支付
- PYMNTS 跨境支付: https://www.pymnts.com/category/news/cross-border-commerce/cross-border-payments/feed/
- FinTech Futures: https://www.fintechfutures.com/feed/

---

## 推送时间表（AEST，23:00-06:30 不推送）

> ⚡ 所有新闻推送任务使用 `delegate` tool 交给 `news_fetcher` 工人执行。
> 主 agent 只做：① 下发 delegate 任务 ② 读取报告 ③ 一句话评价后发 Telegram

### 06:30 — 早报综合

**delegate 指令**（交给 news_fetcher）：
```
抓取以下 RSS 源的最新内容，整理后推送到 Telegram 用户 495916105。

综合/澳洲：
- https://www.abc.net.au/news/feed/51120/rss.xml
- https://www.sbs.com.au/chinese/feed

科技/AI：取 TechCrunch AI、HackerNews AI 各1条精选

天气API（附在消息开头）：
https://api.open-meteo.com/v1/forecast?latitude=-33.88&longitude=151.1&daily=temperature_2m_max,temperature_2m_min,precipitation_sum,weathercode&timezone=Australia%2FSydney&forecast_days=1

时段名：早报综合
```

**主 agent 后续动作**：
- 读 news_fetcher 的执行报告
- 如果有封禁源 → send_telegram 通知爸爸需要更换
- 对推送内容给出一句话评价 → send_telegram 发给爸爸

### 09:30 — 科技/AI/机器人

**delegate 指令**：
```
抓取以下 RSS 源，整理后推送到 Telegram 用户 495916105。

- https://techcrunch.com/category/artificial-intelligence/feed/
- https://hnrss.org/newest?q=AI
- https://hnrss.org/newest?q=robotics
- https://hnrss.org/newest?q=drone
- https://www.reddit.com/r/artificial/.rss
- https://www.reddit.com/r/robotics/.rss

只推AI、机器人、技术突破类内容，水帖跳过。
时段名：科技AI
```

**主 agent 后续动作**：同早报

### 12:30 — 无人机/军事/采购

**delegate 指令**：
```
抓取以下 RSS 源，整理后推送到 Telegram 用户 495916105。

- https://www.reddit.com/r/drones/.rss
- https://www.reddit.com/r/Multicopter/.rss
- https://breakingdefense.com/feed/
- https://www.defensenews.com/arc/outboundfeeds/rss/
- https://www.defenseindustrydaily.com/feed/
- https://www.dsca.mil/RSS
- https://www.janes.com/feeds/news

关注无人机产品、军事采购、国际军贸动态。
时段名：军事无人机
```

**主 agent 后续动作**：同早报

### 15:30 — 中国/政治/科技圈

**delegate 指令**：
```
抓取以下 RSS 源，整理后推送到 Telegram 用户 495916105。

- https://www.reddit.com/r/chinesepolitics/.rss
- https://www.reddit.com/r/Sino/.rss
- https://www.reddit.com/r/China/.rss
- https://36kr.com/feed
- https://www.cnbeta.com.tw/rss.xml
- https://linux.do/c/develop/4.rss
- https://www.v2ex.com/index.xml

关注中国政治军事动态、国内科技圈热议话题。
时段名：中国科技
```

**主 agent 后续动作**：同早报

### 18:30 — 业余无线电/Maker

**delegate 指令**：
```
抓取以下 RSS 源，整理后推送到 Telegram 用户 495916105。

- https://www.reddit.com/r/amateurradio/.rss
- https://hnrss.org/frontpage

关注无线电、SDR、ESP32、硬件开发相关内容。
时段名：无线电Maker
```

**主 agent 后续动作**：同早报

### 21:30 — 金融/跨境支付/Fintech + 澳洲新闻

**delegate 指令**：
```
抓取以下 RSS 源，整理后推送到 Telegram 用户 495916105。

- https://www.pymnts.com/category/news/cross-border-commerce/cross-border-payments/feed/
- https://www.fintechfutures.com/feed/
- https://www.abc.net.au/news/feed/51120/rss.xml
- https://www.sbs.com.au/chinese/feed

关注跨境支付、国际汇款、澳洲本地新闻。
时段名：金融澳洲
```

**主 agent 后续动作**：同早报

---

## 推送格式要求
- 一条消息搞定，不分多条发
- 先主题标题，再列条目（每条一行）
- 标题简短，必要时附一句中文摘要
- 水帖、广告、重复内容跳过
- 只推有价值的信息

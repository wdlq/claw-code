#### 文章目录
P.S. 无意间发现了一个巨牛的人工智能教程，非常通俗易懂，对AI感兴趣的朋友强烈推荐去看看，传送门https://blog.csdn.net/HHX_01

### 前言
我的项目默认接的是 DeepSeek。最近有个事特别闹心——DS 的缓存命中率低得离谱，有时候只有 20% 到 30%。
你们知道 DeepSeek 的 prefix caching 命中的部分价格有多便宜吗？便宜到让我怀疑人生。但命中率这么低，API 费用直接起飞。每个月账单看得我想把显卡卖了换硬币，结果发现硬币更保值。
我在 L 站、B 站 AI 区，经常看到一个叫 **Reasonix** 的 agent harness 被吹爆。人家用 DS 跑，缓存命中率一般能达到 99%。
同样是用 DeepSeek，为什么我的项目命中率这么拉？这就好比你和学霸用同一支笔，人家考满分你考及格。问题不在笔，在你。
于是我决定：扒了它的源码。
别问，问就是开源精神——开源的代码，就是我的代码。我抄代码的速度，比抄作业还快。而且抄得理直气壮，毕竟 LICENSE 都写着 MIT 呢。

### DeepSeek 的缓存机制：一个无情的字符串比较器
DeepSeek 的 prompt caching 是服务端自动的，不用像 Anthropic 那样手动打 `cache_control` 标记。只要多次请求的 prompt 前缀完全一致，命中部分就按缓存价计费。
规则简单粗暴：从 prompt 的第一个 token 开始，逐个往下比。中间任何一个 token 不一样，从这个位置起，后面全部 miss。这就像你排队买奶茶，前面 99 个人都一模一样，第 100 个人突然要加珍珠。对不起，从第 100 个人开始，后面所有人重新排队。
DeepSeek 的缓存策略，本质上就是一个有强迫症的字符串比较器。它不在乎你后面变没变，它只在乎：前面有没有一个字不一样。
这就像你女朋友问你"刚才那句话什么意思"，你说"没什么意思"，然后她就开始从第一句话逐字分析。DeepSeek 也是这个德行，而且比女朋友还绝情——女朋友至少会给你解释的机会，DeepSeek 直接 miss，钱照扣。
所以想让命中率高，核心思路就一个：让 prompt 的**前缀部分** 在每一轮请求里纹丝不动，有变化的部分全部塞到 prompt 的**末尾** 去。
听起来简单对吧？但很多人直接把记忆更新、状态切换往 system prompt 里一塞，缓存当场去世。这就像你装修房子，为了换一盏灯，把整栋楼拆了重建。物业都看傻了，邻居都报警了，DeepSeek 的账单直接把你送走。

### Reasonix 的"记忆约束"：不动前缀，动尾巴
Reasonix 在项目记忆文件 `REASONIX.md` 里写着一条铁律：
Cache-first: the system-prompt prefix (base prompt + tools + memory) must stay byte-stable across turns so DeepSeek’s automatic prefix cache stays warm. Never mutate it mid-session.
翻译成人话：系统提示词前缀（基础提示词 + 工具 + 记忆）在多轮对话中必须保持字节级稳定，确保 DeepSeek 的自动前缀缓存一直热着。中途千万别动它。
这就像你家里的 WiFi 密码，一旦设好了，全家都别改。谁改谁负责重新教所有设备连网。我上次改了个密码，我妈三天没理我，智能音箱也不会说话了，扫地机器人在客厅转了三圈找不到回家的路。

看 Reasonix 的 `internal/boot/boot.go` 里的 `Build()` 函数，前缀的组装顺序是这样的：
```go
// boot.go
sysPrompt, err := cfg.ResolveSystemPromptForRoot(root)
// output style 折叠进 base prompt，只组装一次，进入 cache-stable prefix
if st, ok := outputstyle.Resolve(cfg.Agent.OutputStyle, outputstyle.Dirs()); ok {
    sysPrompt = outputstyle.Apply(sysPrompt, st)
}
sysPrompt += "\n" + config.LanguagePolicy
// 持久记忆在这里折叠进系统提示，一次
mem := memory.Load(memory.Options{CWD: root, UserDir: config.MemoryUserDir()})
sysPrompt = memory.Compose(sysPrompt, mem)
// skill 只放索引（名字+描述），正文按需加载，不进 prefix
skills := skillStore.List()
if !tokenEconomy {
    sysPrompt = skill.ApplyIndex(sysPrompt, skills)
}
```
base prompt 放最前面，这个正常来讲不会变。即使记忆跨 session 变了，base prompt 仍然是一个合法的缓存前缀。
skill 只塞名字和描述进前缀，正文用到的时候才加载，不进 prefix。这个应该是常识了，但常识往往是最容易忘的。就像你知道要早睡，但你依然在凌晨两点刷手机。常识和做到之间，隔着一整个银河系，还有我的黑眼圈。
这个 `sysPrompt` 组装完之后传给 `agent.NewSession`，变成 `Session.Messages[0]`。之后在这个 session 里，它就不会再动了。
工具 schema 也一样，启动时注册进 `*tool.Registry`，session 内不变。连 plan 模式这种功能切换都不改工具列表。进入 plan 模式不会从 schema 里删掉写工具，而是在执行时进行拦截，让模型看到错误自己适应。
这就是细节啊。就像你进餐厅，服务员不会为了让你减肥直接把菜单上的红烧肉涂掉，而是等你点了之后告诉你：“不好意思，今天卖完了。” 模型一脸懵逼，但缓存稳如老狗。DeepSeek 看着模型自己纠错，内心 OS：“这届 AI 挺自觉。”
为了顺应开源精神，我决定 OpenMMV 直接把这个方案给抄过来。抄代码不丢人，抄了还说自己是原创才丢人。我抄得光明正大，README 里还写了 “Inspired by Reasonix”——虽然 “Inspired” 这个词用在这里，约等于 “Ctrl+C Ctrl+V”。

### XML 拼接：把变化赶到 prompt 的尾巴上
问题来了。如果用户在会话中途改了计划模式、临时加了条记忆，模型必须知道这些变化吧？
肯定不能直接写进 system prompt，不然前缀变了，缓存命中直接 gg。这就像你正在直播，突然换了个背景布，观众全跑了。DeepSeek 的缓存比观众还绝情——观众跑了还能回来，缓存 miss 了钱就没了。
Reasonix 的解法是：所有中途会变化的，都作为 XML 块，拼到当前 user 消息的最前面。
源码在 `internal/control/input.go` 的 `Compose()` 里。这段代码我第一眼看到的时候，脑子里只有一个念头：这帮人是不是 XML 成精了？

```go
// input.go —— Compose
func (c *Controller) Compose(text string) string {
    plan := c.planMode
    notes := c.pendingMemory
    goal, goalStatus, goalResearchMode := c.goals.snapshot()
    
    // active goal 块，拼在 user turn 前面
    if strings.TrimSpace(goal) != "" && goalStatus == GoalStatusRunning {
        text = activeGoalBlock(goal, goalResearchMode) + "\n" + text
    }
    
    // plan mode 标记，拼在 user turn 前面
    if plan {
        text = PlanModeMarker + "\n" + text
    }
    
    // 推理语言块，瞬时 user-turn 上下文
    text = agent.WithReasoningLanguage(text, reasoningLanguage)
    
    // 中途加的记忆，搭 turn
    if len(notes) > 0 {
        var b strings.Builder
        b.WriteString("<memory-updates>\n")
        b.WriteString("The following project-memory changes were just made and apply from now on:\n")
        for _, n := range notes {
            b.WriteString("- " + n + "\n")
        }
        b.WriteString("</memory-updates>\n")
        text = b.String() + text
    }
    
    // 后台任务完成通知，搭 turn
    if c.jobs != nil {
        if note := c.jobs.DrainCompletedNoteForSession(c.parentSessionID()); note != "" {
            text = "<job-completed>\n" + note + "\n</job-completed>\n" + text
        }
    }
    return text
}
```

这些 `<memory-updates>`、`PlanModeMarker`、`<job-completed>`、`<active-goal>` 块，全部拼在最新 user 消息前面，处于 prompt 的尾部。
前缀部分是稳定的，能命中缓存；尾部是变化的。
这就像你搬家，不是把整栋楼推倒重建，而是在门口贴个告示：“新地址在隔壁，请查收。” 快递小哥看了直呼内行。DeepSeek 看了也直呼内行：“前缀没变？好，前面的我都知道了，只看尾巴就行。”

注释里写得很清楚：
Memory added mid-session rides the turn (never the cached system prefix), so it takes effect now without invalidating the prompt cache. It folds into the system prefix on the next session, where it costs nothing per turn.
会话中途添加的记忆会附着在当前对话轮次中（绝不触碰已缓存的系统前缀），因此它不仅能立即生效，还不会导致提示词缓存失效。在下一次全新的会话中，它会被合并到系统前缀里，届时由于缓存机制，它在每一轮交互中都不会再产生额外成本。
Reasonix 把会话状态和缓存前缀做了物理隔离。最新状态变化拼到 XML 里，不会直接拼进 system prompt。这操作，就像把垃圾扔到垃圾桶而不是扔到客厅——虽然都是扔，但效果天差地别。我妈看了这段代码都说：“这代码比我收拾房间还讲究。”

### 协议层防 cache miss：DeepSeek 的 thinking 模式陷阱
前面都是 harness 层的设计。在 provider 适配层，Reasonix 还针对 DeepSeek 做了几个直接避免缓存失效和请求失败的处理。
DeepSeek 的 thinking 模式有个巨坑：当你要把历史里某条"带工具调用"的 AI 消息重新发给 DeepSeek 时，必须把这条消息当初的思维链 `reasoning_content` 也一起发回去。不然 DeepSeek 直接返回 400。
400 错误就像你女朋友说"我没事"——表面平静，实则暴风雨前的宁静。DeepSeek 的 400 更直接，直接告诉你：“不行，滚。” 连个解释的机会都不给，比分手还干脆。

Reasonix 在 `buildRequest` 里专门处理了：
```go
// openai.go —— buildRequest
// DeepSeek thinking mode 400s a tool_calls turn whose reasoning_content was
// dropped on a cache-miss replay, so round it back — but only on the turn
// that carries the tool calls.
if c.deepseek && m.Role == provider.RoleAssistant && len(m.ToolCalls) > 0 {
    cm.ReasoningContent = m.ReasoningContent
}
```
注意它只在带 `tool_calls` 的那一轮回传。这样既避免了 400，又不把所有 reasoning 都塞回去——因为重新发送全部 reasoning 会有多余的花费。
这就像你去饭店吃饭，服务员不会因为你没点饮料就把你的筷子收走。细节决定成败，也决定你的钱包厚度。多传一个 reasoning_content 可能就多花几毛钱，积少成多，一个月下来够买杯奶茶了。我算了算，Reasonix 这个优化一年能省下的钱，够我喝三百杯奶茶。三百杯！
这样看起来像是边边角角的逻辑判定，其实就是缓存命中策略的细节。把这些容易踩坑的地方都填平了，缓存命中率才会高上去。
很多人写代码就像拼乐高，只拼主体，不拼边角。结果一碰就散架。Reasonix 是连说明书背面的小字都认真读了。我读代码的时候就在想：这帮人是不是闲得慌？后来发现，闲得慌的人才能把缓存命中率做到 99%。

### 工程纪律：命中率 99% 不是写出来的，是管出来的
命中率要做到 99% 点几，光代码里写好不够，还得保证后续改动不退化。这一层是很多项目缺失的，Reasonix 为此做了几个工程化的规矩。

#### 第一，缓存诊断
agent 的 run loop 里每一轮都取前缀快照，跟上一轮对比，把结果挂到 Usage 事件上。诊断信息里带了 `PrefixChanged`、`PrefixChangeReasons`，告诉你这一轮 miss 是因为 system 变了、工具变了、还是 log 被压缩重写了。
Reasonix 让你能看到为什么这一轮 miss 了。因为看不见就管不住嘛。而且它用的是会话级的累计命中率，不是单轮的比率。压缩时也不重置累计值，所以压缩不会让显示的命中率突然暴跌。这就像你减肥，每天称体重，但只看趋势不看单日波动。某天多吃了一块蛋糕，曲线不会断崖式下跌，你也不会 panic 到把秤砸了。虽然我很想砸。

#### 第二，CI 里的 cache-guard
它有两个脚本。一个跑 `TestReleaseCacheHitGuard` 测试，这是"发版前缓存命中率不能退化"的检测脚本。
另一个脚本的作用是：当一个 PR 改了缓存敏感文件，比如 `internal/boot`、`internal/tool`、`internal/provider` 这些，就强制要求 PR body 里写明 `Cache-impact:` 和 `Cache-guard:` 两行。值用 `n/a`、`none`、`todo` 这种敷衍词会被直接拒绝。
这就像你公司的报销制度，不写清楚用途直接打回。没人喜欢填表，但大家都喜欢钱不被乱花。Reasonix 的 CI 比 HR 还严格，敷衍它？门都没有。我怀疑它的 CI 脚本是用 DeepSeek 写的，因为 DeepSeek 也不接受敷衍。

#### 第三，写进项目记忆的 PR 规则
`REASONIX.md` 把这套规范固化成对所有贡献者的约束。
也就是说，"这个改动会不会影响 DeepSeek 缓存命中率"是每个 PR 必须显式回答的问题。
这就像你结婚前要签婚前协议——虽然麻烦，但能避免以后扯皮。Reasonix 的缓存命中率能长期维持 99% 点几，靠的不是某一行神代码，而是这一整套工程纪律。代码可以抄，但工程纪律抄不来。就像你可以抄学霸的笔记，但抄不来学霸每天六点起床的习惯。我抄过，真的抄不来。我六点起床的唯一原因是尿憋的。

### 独立 session 历史：双模型协作不打架
Reasonix 支持双模型协作：planner 计划 + executor 执行。这是一个很容易缓存崩坏的场景。
如果在同一个会话里切模型，system、tools、历史的格式全变，缓存直接清零。这就像你刚把冰箱温度调好，有人把冰箱门拆了。你问谁拆的，planner 和 executor 互相甩锅。最后发现是合租的锅，而合租的锅，永远甩不清。
Reasonix 的做法是让 planner 和 executor 各自独立 session，不共享历史。planner 用自己的 session 产出计划，计划作为结构化文本交给 executor，executor 在自己独立的 session 执行。
The sessions never mix, so neither model’s prefix is disturbed by the other’s turns.
各个会话完全独立，所以两个模型的提示词前缀互不交织，各自的对话轮次也不会互相打扰。
不共享历史，两个 session 各自的缓存前缀是稳定增长的。
这就像合租，最好的关系是各用各的卫生间。共用？那只会带来灾难。我大学室友共用洗发水，三个月后瓶子里的液体颜色都变了。到现在我都不知道那瓶子里到底是什么。Reasonix 显然吸取了教训：各用各的 session，各缓存各的，谁也不打扰谁。

### 请求结构：上半段稳定，下半段蹦迪
把前面这几层串起来，一次发给 DeepSeek 的请求，prompt 大概是这个结构：
上半段是 **缓存命中区** ，逐字节稳定，整个 session 不变：base prompt、output style、language、memory、skill 索引，加上所有工具 schema，再加上 append-only 增长的历史。
下半段是 **缓存未命中区** ，每轮新增，只是尾部一小段：那些 `<memory-updates>`、`<active-goal>`、`<job-completed>` 块，加上 Plan mode 标记，最后才是用户真正输入的文本。
DeepSeek 从头匹配，上面整块前缀和上一轮是一样的，会大面积命中；只有最底下这轮新拼的尾巴是 cache miss。
这就像你去 KTV，上半段是固定的歌单，下半段是现场点歌。点歌的人换来换去，但歌单本身不会变。除非有人把歌单撕了，那缓存就真 miss 了。我有一次在 KTV 把歌单撕了，不是因为缓存，是因为有人点了《学猫叫》十遍。

### 写在最后
有时候我在想，为什么 OpenMMV 的缓存命中率这么小。分析了一下，因为 OpenMMV 的 AI 消息的 token 量太少了，前缀本身就很小，加之每轮的新增消息占比比较大，缓存命中率自然不会太大。
前缀占比越大，命中率就越逼近 100%。Reasonix 的长会话里 system 加 tools 加历史可能有几万甚至几十万 token，而尾巴那一小段只有几百到几千 token，这样缓存命中率就比较大。这就像你请客吃饭，请的人越多，人均越便宜。缓存也是这个理——前缀越大，尾巴的占比越小，miss 的成本就越低。我请三个人吃饭人均 100，请三十个人人均 30。DeepSeek 的缓存账单也是这个算法，而且 DeepSeek 不会跟你 AA。
高缓存命中率不是某一行代码的技巧，而是从项目记忆到 CI 脚本一以贯之的工程纪律。
如果你觉得这篇文章有帮助，点赞关注，点点赞~
毕竟，我扒源码的时候，咖啡都喝了三杯。现在心跳比缓存命中率还高。如果这篇文章的缓存命中率也能到 99%，那我下次写文就不用喝咖啡了——直接喝缓存命中率的喜悦。

P.S. 无意间发现了一个巨牛的人工智能教程，非常通俗易懂，对AI感兴趣的朋友强烈推荐去看看，传送门https://blog.csdn.net/HHX_01
# Puxiri — Plano completo de desenvolvimento

Versão: 1.0 · Data: 02/10/2026 · Status: especificação para iniciar a implementação.

## 1. Produto e decisões fechadas

Puxiri é um gestor de trabalho self hosted, inspirado no Kanban do Jira e na agilidade do Linear. Cada instalação atende uma organização, com usuários, convidados e vários projetos. O objetivo é baixo custo de operação, instalação simples, segurança de acesso e boa experiência de uso.

| Decisão | Definição |
|---|---|
| Produto | Puxiri |
| Repositório genérico | `work-tracker` |
| Executável | `puxiri` |
| Backend | Rust, Axum e Tokio |
| Banco | SQLite em disco local, com WAL |
| Acesso ao banco | SQL explícito com SQLx; integração de backup isolada |
| Frontend | Svelte, TypeScript e Vite; SPA embutida no executável |
| Jobs | No mesmo processo, persistidos no SQLite |
| Deploy | Binário único ou Docker com volume persistente |
| Organização | Uma por instalação; sem SaaS multi tenant no início |
| Configuração | UI administrativa; bootstrap mínimo no ambiente |
| Emails | Resend e Cloudflare Email Service via REST |
| CAPTCHA | Turnstile, reCAPTCHA ou hCaptcha |
| Login externo | Google, Apple, GitHub e GitLab |
| Backup remoto | API compatível com S3 |
| MCP | Transporte HTTP e opção stdio pelo mesmo binário |

Nome escolhido pelo usuário. Registro de marca, domínio e disponibilidade de namespaces não estão confirmados. Isso não impede trabalhar no repositório genérico.

### 1.1 Escopo da primeira versão completa

- Setup e restauração de nova instância.
- Organização, membros, convidados, convites e projetos.
- Kanban, issues, tipos, status, responsável, prioridade e data de entrega.
- Campos customizados tipados, busca, filtros, comentários e histórico.
- Autenticação, autorização, sessões, MFA administrativo, bloqueio de contas.
- Login por senha configurável e quatro provedores externos.
- Controle de cadastro, emails temporários e CAPTCHA.
- Emails transacionais com fila persistente.
- MCP com autorização e trilha de alterações.
- Automações por evento e por agendamento.
- Painel operacional, métricas agregadas, auditoria, backup, exportação e restauração.
- Design system, temas claro/escuro, teclado e acessibilidade.

### 1.2 Fora do escopo inicial

Sprints, story points, roadmap, Gantt, dependências entre issues, cobrança, apps nativos, anexos, time tracking, fórmulas em campos, scripts arbitrários em automações, integração bidirecional com Git e edição colaborativa de descrição. Um tipo “épico” não cria hierarquia automaticamente. Não incluir IA obrigatória nem telemetria externa obrigatória.

## 2. Arquitetura e custo

Um processo serve arquivos estáticos, API, MCP, scheduler e workers. Um banco mantém conteúdo, configuração, sessões, jobs e auditoria. Não introduzir Redis, NATS, PostgreSQL ou servidor Node em produção.

Fluxo de uma mutação: transporte → autenticação → autorização → serviço da funcionalidade → transação SQLite → issue/histórico/outbox → commit → processamento assíncrono. Eventos nunca devem ser publicados antes do commit. Chamadas a provedores externos nunca devem acontecer dentro de uma transação de escrita.

Frontend, REST, MCP e automações chamam a mesma camada de aplicação. Evitar duplicar regras em handlers ou criar uma abstração genérica de repositório para cada tabela.

### 2.1 SQLite

- Banco e arquivos WAL/SHM no mesmo volume local persistente. Não montar o banco em NFS nem em S3.
- WAL, foreign keys habilitadas em toda conexão, busy timeout inicial de 5 segundos e transações curtas.
- `synchronous=FULL` como padrão de durabilidade; mudanças ficam em configuração operacional avançada, documentadas.
- Pool pequeno, inicialmente quatro conexões, validado em benchmark. Há apenas um escritor por vez; mais conexões não criam escrita paralela.
- Evitar consultas longas que impeçam avanço do checkpoint; monitorar tamanho do WAL.
- Migrations versionadas, checksums e backup consistente antes de upgrades relevantes.
- SQLx com queries parametrizadas. Campos de ordenação e filtros dinâmicos passam por allowlist.
- FTS5 para títulos e descrições. Aplicar autorização na consulta antes de retornar resultados, contagens ou snippets.
- Índices nas FKs e caminhos usados em filtros. EXPLAIN QUERY PLAN nos caminhos críticos.
- Apenas uma instância ativa do processo por diretório de dados; lock explícito no startup.

Para a API de backup, escolher binding compatível com a versão SQLite usada pelo SQLx. Se utilizar rusqlite, garantir compatibilidade de `libsqlite3-sys` no build. Executar trabalho bloqueante fora do runtime assíncrono. Esse ponto exige um spike antes de fechar dependências.

### 2.2 Limites e metas

Não prometer capacidade por número de usuários sem carga medida. Ambiente de referência inicial: Linux, 1 vCPU, 1 GB RAM e SSD local; testar também 2 vCPU/2 GB. Dataset de validação: 100 mil issues, vários projetos e campos, com distribuição realista de comentários e eventos.

Metas propostas, sujeitas ao benchmark: leitura comum p95 abaixo de 200 ms no servidor; mutação comum p95 abaixo de 300 ms; 20 usuários simultâneos com baixa taxa de erro; processo ocioso abaixo de 150 MB RSS. Medir separadamente uso normal, automações, busca e backup concorrente. Metas não são garantias de capacidade.

Custo recorrente: servidor, disco, armazenamento de backups e email. Não estimar preços dos provedores sem cotação atual. O software não exige banco gerenciado ou serviço de fila.

## 3. Estrutura do repositório

Um Cargo package com biblioteca e executável. Dividir em crates apenas quando houver motivo concreto.

| Caminho | Conteúdo |
|---|---|
| `src/main.rs` | CLI, inicialização e encerramento |
| `src/lib.rs` | Composição e módulos |
| `src/config/` | Bootstrap, configurações persistidas e criptografia |
| `src/db/` | Pool, migrations e helpers transacionais |
| `src/auth/` | Login, identidades, MFA, sessões e recuperação |
| `src/access/` | Políticas e principais autenticados |
| `src/projects/`, `src/issues/`, `src/fields/` | Funcionalidades centrais |
| `src/automation/`, `src/jobs/`, `src/events/` | Regras, scheduler e outbox |
| `src/integrations/` | Email, CAPTCHA, S3 e provedores de identidade |
| `src/mcp/`, `src/http/` | Transportes e contratos |
| `src/admin/`, `src/backup/` | Operação e recuperação |
| `migrations/` | SQL incremental |
| `web/` | Svelte SPA e tokens visuais |
| `tests/` | Integração, segurança e recuperação |
| `docs/` | Arquitetura, operação, API e decisões |
| `deploy/` | Dockerfile, Compose e exemplo systemd |

Dependências candidatas: axum, tokio, tower-http, sqlx, serde, tracing, reqwest, argon2, biblioteca de criptografia autenticada, oauth2/openidconnect, SDK oficial Rust do MCP quando compatível e cliente S3. Fixar versões no lockfile após conferir documentação, manutenção e compatibilidade; este plano não fixa versões de crates.

Build do frontend com dependências travadas, geração dos assets e embedding via rust-embed ou include_dir. A execução em produção não precisa de Node. Assets locais, incluindo fontes; nada carregado obrigatoriamente de CDN.

## 4. Modelo de dados

IDs internos opacos; UUIDv7 como escolha inicial. Datas de instantes em UTC; data de entrega como `YYYY-MM-DD`, sem converter em timestamp. Timezone IANA da organização define vencimento e agendas; padrão sugerido `America/Sao_Paulo`, configurável. Timestamps de auditoria não mudam conforme o timezone.

| Entidade | Campos/regras centrais |
|---|---|
| organization | Singleton, nome, locale, timezone |
| users | Nome, estado active/banned/disabled, timestamps; não deletar autoria ao banir |
| user_emails | Email normalizado, verificação, principal; política de unicidade explícita |
| auth_identities | User, provider, issuer, subject; unique por identidade externa |
| password_credentials | Hash Argon2id e data de alteração |
| mfa_credentials / recovery_codes | TOTP criptografado e códigos de uso único hashed |
| sessions | Hash do token, usuário, expiração, atividade, revogação |
| one_time_tokens | Hash, finalidade, destinatário, expiração e consumo |
| organization_members | Papel owner/admin/member/guest |
| installation_admins | Papel operacional separado da membership |
| invites | Hash do convite, email, projeto/papel permitido, expiry |
| projects | Chave estável, nome, descrição, archived_at, next_issue_number |
| project_members | User e papel manager/member/viewer |
| issue_types | Projeto, nome, ícone, cor, archived_at |
| statuses | Projeto, nome, categoria backlog/active/done/canceled e ordem |
| issues | Projeto, número, título, descrição, tipo, status, responsável, criador, prioridade, due_date, rank, version e timestamps |
| custom_fields | Projeto, chave imutável, nome, tipo, required, visible_on_card |
| custom_field_options | Campo, ID estável, label, ordem, archived_at |
| issue_field_values | Issue/campo únicos; colunas text/number/date/bool/option com CHECK de tipo |
| issue_multi_options | Issue/campo/opção; integridade referencial |
| comments | Issue, autor, corpo, version, timestamps |
| issue_events | Histórico funcional autorizado pelo projeto, ator e diff tipado |
| saved_views | Filtros validados, usuário ou projeto |
| api_tokens | Hash, owner, scopes, projetos permitidos, expiry e revoked_at |
| automation_rules | Projeto, gatilho, condições e ações em JSON versionado, enabled, run_as |
| automation_runs / action_runs | Evento, regra/versão, tentativas, resultado e idempotência |
| outbox_events / jobs | Payload versionado, estado, disponibilidade, lease e tentativas |
| recurring_schedules | Timezone, agenda, próximo horário e política de missed runs |
| notification_deliveries | Destinatário, template, provider, status, erro redigido |
| settings / secret_values | Dados tipados, ciphertext, versão da chave |
| backup_runs / backup_objects | Estados, manifesto, destino, tamanho e checksum |
| audit_events | Operações administrativas sem conteúdo privado |
| usage_daily | Agregados com retenção configurável |
| oauth_clients / oauth_grants | Clientes MCP, consentimentos, scopes e revogação |

Schema efetivo será escrito por migration e revisado. FKs compostas ou validação transacional garantem que tipo, status, campos, opções e responsáveis pertençam ao mesmo projeto. Não confiar em IDs fornecidos pelo cliente.

Sem apagar status com issues: exigir migração para outro status. Tipo ou campo em uso é arquivado; mudança de tipo do campo exige migração explícita. Ao tornar campo obrigatório, exigir preenchimento dos dados existentes ou cancelar a operação com relatório das pendências. Backup mantém valores arquivados.

## 5. Issues e Kanban

### 5.1 Regras

- Chaves de projeto como `DEV`; identificadores `DEV-123` gerados em transação, nunca reutilizados. Chave fica imutável no v1.
- Título obrigatório, limite inicial 240 caracteres. Descrição e comentários em Markdown com HTML bruto desabilitado e links sanitizados; limite inicial 64 KB por corpo.
- Prioridades none/low/medium/high/urgent. Um responsável opcional com acesso ao projeto.
- Um board por projeto; cada coluna equivale a um status.
- Categoria done define conclusão. Reabrir limpa `completed_at`; cancelamento tem categoria própria.
- Vencimento: data anterior ao dia atual da organização e categoria diferente de done/canceled. Não mover automaticamente issues atrasadas sem regra configurada.
- Sem permissão por issue no v1: todo conteúdo de uma issue herda acesso do projeto.
- Exclusão por lixeira com retenção configurável, padrão 30 dias. Purge é job autorizado e auditado.

### 5.2 Ordenação e concorrência

Rank inteiro espaçado por coluna como solução inicial. Inserir entre vizinhos; quando não houver espaço, rebalancear em transação. Cliente envia vizinhos e versão, não rank arbitrário. Aplicar move, rank, version, histórico e outbox atomicamente.

Atualizações usam versão esperada; conflito retorna HTTP 409 com instrução para recarregar. UI mostra conflito e preserva rascunho. Idempotency-Key em criação/mutações suscetíveis a retry, escopo por principal/operação; mesma chave com payload diferente retorna conflito.

Drag and drop otimista com rollback visual em falha. Oferecer mover por menu/teclado. Carregar colunas em lotes, com contagens e “carregar mais”; não renderizar milhares de cards de uma vez. Preservar filtros na URL e scroll ao fechar issue.

### 5.3 Busca e filtros

FTS por título/descrição, chave exata, filtros por projeto, tipo, status, responsável, prioridade, prazo e campos customizados. Paginação por cursor com desempate por ID. Contagens e facetas respeitam acesso. Sem busca em projetos privados a que o usuário não tem acesso.

## 6. Autorização e privacidade

Política deny by default. Cada endpoint, ferramenta MCP, job e download verifica principal, estado da conta e acesso atual. Alterar papel invalida caches de acesso; não confiar em claims de longa duração.

| Ação | Installation admin | Org owner/admin | Project manager | Member | Viewer |
|---|---|---|---|---|---|
| Configurar auth/email/CAPTCHA/backup | Sim | Só se também installation admin | Não | Não | Não |
| Saúde e métricas agregadas | Sim | Não por padrão | Não | Não | Não |
| Gerenciar membros da organização | Não por esse papel | Sim | Não | Não | Não |
| Criar projeto | Não por esse papel | Sim | Não por padrão | Não por padrão | Não |
| Consultar conteúdo | Só se membro do projeto | Só se membro do projeto | Sim | Sim | Sim |
| Criar/editar/mover issues e comentar | Só com papel de projeto | Só com papel de projeto | Sim | Sim | Não |
| Campos/status/tipos/automações | Só com papel de projeto | Só com papel de projeto | Sim | Não | Não |
| Gerenciar membros do projeto | Não por esse papel | Sim | Sim, sem elevar a org admin | Não | Não |
| Banir usuário da instalação | Sim | Só se installation admin | Não | Não | Não |
| Exportar conteúdo de projeto | Só com papel habilitado | Só com papel habilitado | Sim | Não por padrão | Não |
| Baixar backup completo | Sim + reautenticação/MFA | Só se installation admin | Não | Não | Não |

Org admin pode controlar membership dos projetos, mas não recebe leitura automática. Conceder acesso a si mesmo deixa evento de auditoria. Manter ao menos um org owner ativo e um installation admin ativo; transferir responsabilidades antes de remover/bloquear o último.

Painel operacional lista identidades necessárias à gestão, estado, papéis, contagens e consumo. Não exibe títulos, textos, comentários, resultados de busca, tokens nem payloads de automações. Sem SQL console ou impersonação no v1.

Backups completos contêm conteúdo privado. Criptografia protege armazenamento/transporte, mas não impede o administrador autorizado a restaurá-los. Quem controla host, chave ou memória do processo pode acessar dados. Não apresentar o produto como ponta a ponta.

Banimento revoga sessões, tokens, grants MCP e impede login futuro por qualquer identidade. Interromper novas execuções sob aquele usuário; jobs revalidam acesso ao executar. Permitir desfazer banimento, sem ressuscitar tokens antigos. Exigir motivo e auditar.

## 7. Autenticação, cadastro e recuperação

### 7.1 Setup seguro

Instância vazia inicia em modo setup. Gerar token aleatório de uso único no console local; nunca permitir que o primeiro visitante público se torne administrador. Wizard exige esse token. Rota setup deixa de existir após finalização.

Wizard: criar organização e administrador → criar kit de recuperação → configurar URL canônica e timezone → email → método de login → política de cadastro/CAPTCHA → backup → testar → finalizar. Provedores externos começam desativados. Cadastro padrão: convite. MFA TOTP obrigatório para installation admins após bootstrap; códigos de recuperação de uso único.

### 7.2 Sessões e senha

- Sessões opacas aleatórias com pelo menos 256 bits; armazenar apenas hash do token.
- Cookie Secure e HttpOnly, SameSite=Lax no fluxo comum; política específica para callback Apple por POST deve ser implementada e testada sem enfraquecer todo o site.
- Rotacionar ID após login e elevação. Expiração inicial: 7 dias absolutos e 24 horas de inatividade, configuráveis. Atualizar atividade em intervalos para reduzir escrita.
- Sessão administrativa privilegiada: reautenticação/MFA recente, janela inicial 10 minutos.
- Senha com Argon2id calibrado no hardware alvo; permitir passphrases, sem regras artificiais de composição. Limites contra abuso de custo computacional.
- CSRF em mutações com cookie, validação de Origin, CORS restrito e proteção contra open redirect.
- Verificação de email e reset com tokens aleatórios hashed, expiração e uso único. Respostas genéricas evitam enumeração.
- Reset de senha revoga outras sessões; senha desativada impede login e reset de senha como rota de acesso alternativa.
- UI “minhas sessões”, encerramento individual e global. Nunca armazenar token de sessão em localStorage.

Rate limits por IP confiável, identidade e instalação, com backoff e limites de concorrência. Proxy headers só de proxies explicitamente confiáveis. Não usar bloqueio permanente por tentativas como ferramenta de negação de serviço. Estado crítico persiste para sobreviver a restart; contadores volumosos podem ser agregados em memória com flush limitado.

### 7.3 Login externo

Authorization Code e PKCE onde suportado; state de uso único; nonce e validação issuer/audience/assinatura/expiração para OIDC. Identidade é provider + issuer + subject estável. Não usar email como identidade externa nem vincular contas automaticamente por coincidência de email.

| Provider | Configuração na UI | Particularidades a validar |
|---|---|---|
| Google | Client ID, secret, callback copiado | Claims e verificação de email |
| Apple | Services ID, Team ID, Key ID, chave privada | Client secret assinado/renovado, callback POST, private relay, nome apenas no primeiro consentimento |
| GitHub | Client ID e secret | OAuth, ID estável e emails verificados via API; email pode faltar |
| GitLab | Client ID, secret e issuer/base URL | GitLab.com inicialmente; self hosted permitido com endpoint validado e aprovação explícita |

Provider retornando email ausente/não verificado exige coleta e verificação local quando necessário. Apple relay não deve ser classificado automaticamente como email descartável. Não guardar access tokens do provider após resolver identidade se nenhuma funcionalidade precisar deles.

Vincular provider exige sessão atual e reautenticação; desvincular exige método alternativo funcional. Redirect pós-login aceita apenas caminhos internos. Provedores configuráveis não devem permitir discovery irrestrita a endpoints privados inesperados.

### 7.4 Cadastro e emails temporários

Modos: disabled / invite_only / open. Essas regras valem para senha e login externo. Usuário já existente continua podendo entrar quando cadastro está fechado. Login externo de pessoa nova não cria membership automaticamente quando convite é obrigatório.

Emails temporários: permitir/bloquear na UI; lista local versionada, importação, atualização opcional, allowlist/denylist e diagnóstico do motivo. Não consultar serviço externo com email completo. Alterar política não bane usuários existentes automaticamente. Lista é heurística e não prova identidade; exceções explícitas precedem a lista genérica.

### 7.5 CAPTCHA

Um provider ativo por vez. Suportar Turnstile, hCaptcha e reCAPTCHA com variante explicitamente selecionada; no v1 implementar reCAPTCHA v2 checkbox, deixando v3/Enterprise fora do adaptador inicial.

Configurar site key, secret, domínios, idioma e fluxos: cadastro, recuperação e login suspeito. Verificar token no backend, hostname/action quando aplicável, expiração e reuse conforme contrato do provider. Nunca confiar no frontend. Falha de verificação bloqueia o fluxo público protegido, com mensagem de indisponibilidade. Clientes API/MCP usam autenticação e limites próprios.

Prever desativação emergencial via CLI local auditada se uma configuração incorreta impedir acesso. Não disponibilizar bypass público. CSP permite apenas os domínios necessários ao CAPTCHA escolhido.

### 7.6 Evitar lockout

Desativar senha exige provider testado e vínculo funcional dos administradores afetados. Verificar usuários dependentes de senha e mostrar impacto antes de aplicar; não religar senha silenciosamente. Configuração OAuth/CAPTCHA passa por draft → test → activate, com rollback de settings.

CLI `puxiri admin recover` usa acesso ao host e material de recuperação para emitir acesso administrativo temporário; registra evento. Não cria uma senha pública permanente. Acesso de recuperação não substitui posse da chave que descriptografa segredos antigos.

## 8. Configurações e segredos

Tudo que é configuração funcional fica na UI. Exceções de bootstrap: endereço de bind, diretório de dados, caminho da master key, proxies confiáveis e parâmetros de logging. São necessários antes de o servidor e banco existirem.

Se não houver chave fornecida, gerar uma master key criptográfica e salvar fora do SQLite, arquivo com permissões restritas no diretório de dados. Oferecer montagem separada por arquivo/secret em produção. Wizard exige exportação do kit de recuperação antes de habilitar backups criptografados.

Segredos no banco com criptografia autenticada e AAD vinculada à entidade/versão. Campos mascarados, valor plaintext nunca retornado após salvar. Logs redigidos; auditoria registra nomes alterados, não valores. Rotação mantém key IDs e requer estratégia para backups antigos.

URL canônica controlada pelo admin; não gerar links de recuperação a partir de Host não confiável. Alterações sensíveis exigem MFA recente. Testes de conexão não retornam credenciais em erros.

## 9. Emails transacionais

Adapter comum com `send`, `test_connection`, classificação de erro e registro de provider_message_id. Resend via API; Cloudflare Email Service via REST, sem Worker intermediário. Isso não equivale a usar Email Routing para envio.

UI: provider, API token, remetente, nome, reply-to; Cloudflare inclui Account ID. Indicar requisitos de domínio e habilitação da conta. Disponibilidade, quotas e condições comerciais devem ser verificadas na integração; suporte no software não concede acesso ao serviço.

Templates: convite, confirmação de email, recuperação, alteração sensível, atribuição e alerta operacional. HTML e texto simples; PT-BR inicialmente, chaves preparadas para inglês. Prévia e email de teste sem salvar configuração inválida.

Fila no SQLite com backoff/jitter, classificação entre erro temporário/permanente e limite de tentativas. Idempotência quando suportada pelo provider. Em timeout ambíguo, pode haver duplicação; não prometer exactly once externo. Tokens podem ser reemitidos e invalidados sem revelar conteúdo em logs.

Proibir destinatário arbitrário em automação no v1: ações de notificação usam membros autorizados. Emails sobre issues privadas revalidam permissão antes do envio. Alertas de falha de email aparecem na UI mesmo quando o próprio email está quebrado.

## 10. MCP e API

### 10.1 Contrato comum

REST em `/api/v1`; OpenAPI gerado. Erros JSON com code, message, request_id, field_errors e retryable. Status 401/403/404/409/422/429 usados de forma consistente; acesso a recurso privado desconhecido retorna 404 para não revelar existência.

API tokens criados na UI, mostrados uma vez, armazenados hashed, com nome, validade e scopes por projeto. Scope efetivo é interseção entre token, acesso atual do owner e operação. Nunca utilizar cookies web como autenticação automática de integrações externas.

### 10.2 Ferramentas MCP

| Tool | Função |
|---|---|
| `list_projects` | Apenas projetos acessíveis |
| `get_project_schema` | Status, tipos, campos, opções e validações |
| `search_issues` | Filtros e cursor |
| `get_issue` | Conteúdo e histórico permitido |
| `create_issue` | Criar com chave de idempotência |
| `update_issue` | Alterar com versão esperada |
| `move_issue` | Status e posição com vizinhos |
| `add_comment` | Comentário com idempotência |

Não expor backup, banimento, configuração de auth ou secrets como ferramentas MCP no v1. Não usar annotations das tools como controle de acesso. Conteúdo das issues é dado não confiável, inclusive quando contém instruções para agentes.

HTTP remoto protegido com autorização OAuth compatível com a revisão estável do MCP escolhida no início da implementação. Implementar protected resource metadata, discovery do authorization server, Authorization Code + PKCE S256, consentimento, audience/resource binding, revogação e refresh rotation. Login Google/Apple/GitHub/GitLab autentica o usuário no Puxiri; tokens desses providers não autorizam diretamente o MCP.

Para manter compatibilidade com clientes simples, permitir token pessoal Bearer quando o cliente suportar configuração manual. Isso não substitui o fluxo OAuth para clientes que o exigem. CLI `puxiri mcp stdio` é modo cliente que conecta à API da instância e usa credencial local protegida; nunca abrir o SQLite diretamente por um segundo caminho sem autorização.

Evitar implementar protocolo OAuth na mão: realizar spike com biblioteca mantida, testes de conformidade e registro explícito de clientes. Cadastro de clients inicialmente controlado pela UI; permitir clientes públicos com PKCE sem tratar client secret como prova para apps desktop. Validar redirects exatos; integração com cliente que exige registro dinâmico entra somente com políticas e testes apropriados.

Limites de payload/paginação/concorrência, validação Origin/Host e não passar tokens recebidos para serviços terceiros. Histórico inclui usuário, client/token, source=mcp e request_id.

### 10.3 Famílias de endpoints

`/auth`, `/me`, `/sessions`, `/invites`, `/organization/members`, `/projects`, `/projects/:id/schema`, `/issues`, `/issues/:id/comments`, `/issues/:id/history`, `/api-tokens`, `/automations`, `/admin/settings`, `/admin/users`, `/admin/health`, `/admin/usage`, `/admin/backups` e `/mcp`.

Fluxo restore isolado em `/setup/restore` somente antes de a instância estar operacional. API e UI não executam SQL arbitrário.

## 11. Automações e scheduler

### 11.1 Configuração

Editor de regras: nome → gatilho → condições → ações → preview → ativar. Condições AND inicialmente; operadores dependem do tipo do campo. JSON versionado com schema de validação; nada de linguagem de programação embutida.

Gatilhos: issue criada, atualizada, status alterado, campo alterado, prazo alcançado, prazo ultrapassado e agenda. Ações: criar issue, mudar status, definir campos, atribuir responsável, comentar, notificar e webhook.

Comparar valores antigos/novos; ação sem mudança real não gera novo evento. Agenda usa timezone IANA e ocorrência única. Política inicial para downtime: consolidar tarefas recorrentes em uma ocorrência pendente, sem criar centenas após restart. UI informa essa política. Em horário inexistente por DST, próxima hora válida; horário repetido executa uma vez por ocorrência local.

### 11.2 Execução confiável

- Persistir outbox na mesma transação da mudança.
- Consumidor agenda jobs com chave única event/rule/version/action.
- Jobs pending/running/succeeded/failed/canceled, lease com vencimento e recuperação após crash.
- Revalidar usuário `run_as` e acesso no momento da execução. Desativar conta ou perder acesso interrompe regra com erro visível.
- No v1 `run_as` é manager que criou a regra; mudança de owner exige aceitação do novo manager. UI avisa dependência antes de remover usuário.
- Cadeia com correlation_id, depth máximo inicial 5 e limite de execuções por cadeia/projeto.
- Limites iniciais configuráveis: 5 ações por regra e orçamento de execução; retries apenas para falhas transitórias.
- Operações locais e registro de resultado feitos atomicamente. Reexecução não duplica issue/comentário.
- Efeitos externos são at least once; timeout ambíguo e deduplicação devem ser visíveis.
- Histórico de execuções mostra erro redigido e botão de retry autorizado.

### 11.3 Webhooks

HTTPS por padrão, segredo de assinatura HMAC, timestamp e event ID. Credenciais mascaradas. Proibir loopback, metadata cloud e destinos privados por padrão; validar DNS, IP e redirects em cada envio para mitigar SSRF. Destino interno só mediante allowlist específica do installation admin. Limitar tamanho de resposta, timeout e redirecionamentos. Não enviar dados privados fora do projeto sem autorização clara do manager.

## 12. Backups online, exportação e restauração

### 12.1 Configuração S3 na UI

Endpoint, região, bucket, prefixo, access key, secret, path-style/virtual-hosted, agenda, retenção, timeout e criptografia. Testar PUT/HEAD/GET/DELETE em prefixo de teste, com cleanup e resultado por capacidade; se delete for negado, declarar retenção como não aplicável pelo app. Credenciais restritas ao prefixo dedicado.

Destinos AWS S3, R2, MinIO e outros são candidatos; validar cada um em testes de compatibilidade. TLS validado. MinIO HTTP em rede local somente opção explícita de desenvolvimento. Não configurar ACL pública; bucket deve ser privado.

### 12.2 Pipeline de backup

1. Reservar execução única e validar espaço local.
2. Criar snapshot via SQLite Online Backup API, por passos com retry/budget, sem copiar arquivo ativo.
3. Fechar snapshot e validar integridade em conexão dedicada.
4. Criar manifesto: versão de formato, app/schema, ID da instância, horário UTC, tamanho, checksum e key ID.
5. Comprimir e criptografar bundle por formato mantido e autenticado, por exemplo age com destinatário público da instalação. Streaming limita consumo de memória.
6. Upload com chave única para o prefixo dedicado; checksum explícito, não tratar ETag como SHA-256 universal.
7. Publicar manifesto de conclusão somente depois de upload completo e verificação disponível no destino.
8. Marcar sucesso; aplicar retenção apenas sobre backups completos reconhecidos por esta instalação.
9. Limpar temporários; nunca apagar último backup válido ao falhar a nova execução.

Não incluir master key plaintext dentro do backup. Kit de recuperação offline contém chave de configuração e chave privada de recuperação dos bundles, com identificação/versionamento. Chaves de backup separadas dos tokens de acesso S3. Restaurar exige bundle e kit; permitir digitar/importar chaves no wizard por TLS e mantê-las fora de logs.

Retenção inicial sugerida: 7 diários, 4 semanais e 3 mensais, configurável. Agendamento diário; RPO nominal até 24 horas condicionado a execução bem-sucedida. RTO só pode ser estimado após ensaio com tamanho real. Mostrar atraso do último backup válido como alerta.

Backup completo captura um ponto consistente do banco, não inclui binário. UI distingue exportação de projeto (JSON/CSV, sem credenciais) de backup integral criptografado. Download integral exige MFA recente, expira e não usa URL pública permanente.

### 12.3 Restauração em nova instância

1. Instância vazia e token local de setup; nenhum cadastro aberto.
2. Escolher upload de arquivo ou S3 configurado temporariamente, sem persistir credenciais antes de validar.
3. Importar material de recuperação, descriptografar para staging, limitar tamanhos e prevenir path traversal/decompression bomb.
4. Validar autenticação criptográfica, checksum, integridade, formato e schema. Recusar backup de versão futura incompatível.
5. Manter original; aplicar migrations no staging somente quando caminho suportado.
6. Ativar banco com troca atômica e servidor em modo recovery, sem tráfego funcional concorrente.
7. Revogar sessões, invites/reset tokens, tokens API/MCP e OAuth grants. Preservar identidades e conteúdo; exigir login novo e recriação de integrações externas.
8. Pausar regras, agendas, webhooks e entregas pendentes. Não repetir emails e efeitos externos antigos. Jobs de email antigos são cancelados; trabalho local pendente fica para revisão.
9. Reconfigurar URL/callbacks, verificar email/CAPTCHA/destino backup, trocar segredos que possam ter sido expostos.
10. Gerar novo ID operacional de instância/prefixo de backup para não disputar retenção com a original.
11. Admin revisa pendências e libera operação. Registrar origem e recibo de restauração sem conteúdo privado.

Não substituir banco de instância ativa pela UI no v1. Recuperação de desastre do mesmo host segue runbook offline, mantendo o original e reutilizando o wizard de recuperação. Testar restore em ambiente isolado sem saída para provedores.

## 13. Área administrativa e observabilidade

| Tela | Conteúdo |
|---|---|
| Visão geral | Estado, versão, uptime, alertas e últimas falhas |
| Saúde | DB, disco/WAL, workers, backlog e probes de integração |
| Usuários | Identidade necessária, estado, papéis, sessões e banimento |
| Entidades | Metadados operacionais de organização/projetos; sem leitor de conteúdo |
| Autenticação | Providers, senha, MFA, sessões, cadastro e emails temporários |
| Email e CAPTCHA | Configuração, testes e status |
| Backups | Agenda, retenção, execuções, download e instruções de restore |
| Uso | Agregados por período, recursos e chamadas |
| Auditoria | Ator, operação, alvo opaco, resultado e horário |

Health público retorna apenas ready/not ready. Diagnóstico detalhado autenticado. `live` indica processo; `ready` indica capacidade de servir o app, banco e migrations. Falha de email ou S3 degrada recurso específico, sem retirar todo Kanban de readiness.

Métricas: latência/erros HTTP por rota, RSS, CPU quando disponível, disco, DB/WAL, busy errors, jobs pendentes/atrasados, duração de automações, backups e chamadas MCP. Sem labels de alta cardinalidade com user ID/issue ID e sem título ou email em métricas. Endpoint Prometheus opcional, autenticado ou restrito à rede local.

Uso: usuários ativos por dia, issues criadas/concluídas, projetos, jobs e chamadas; sem rastrear conteúdo privado ou enviar telemetria externa. Retenção inicial: agregados 12 meses, logs de delivery/jobs 30 dias e auditoria 180 dias, configuráveis. Logs estruturados com request_id e redaction central.

Administrar entidade não é editar tabelas diretamente. Toda operação administrativa passa por caso de uso específico, valida invariantes e registra auditoria.

## 14. Branding e design system

### 14.1 Direção

Marca Puxiri, referência à semente amazônica. Interface calma, rápida e precisa. Identidade regional aparece em cor e símbolo; não inventar significado indígena, nem usar grafismos culturais sem pesquisa. Evitar transformar o dashboard em ilustração temática.

Logo vetorial original: semente abstrata com duas formas curvas; legível em 16–24 px e em monocromia. Marca textual “Puxiri”, sem acento. Slogan provisório: “Trabalho em movimento”.

### 14.2 Tokens iniciais

| Token | Claro | Escuro |
|---|---|---|
| Background | `#F7F8F5` | `#101714` |
| Surface | `#FFFFFF` | `#18221D` |
| Surface elevated | `#EEF2ED` | `#202D25` |
| Texto principal | `#17231C` | `#EDF4EE` |
| Texto secundário | `#506357` | `#B1C1B5` |
| Border | `#D4DED5` | `#354B3D` |
| Primary | `#245C3A` | `#9BD0A8` |
| Texto sobre primary | `#FFFFFF` | `#102718` |
| Accent | `#A45B0B` | `#EDBC6A` |
| Danger | `#B42318` | `#FFB4AB` |

Paleta proposta, não certificada: medir contraste de cada combinação real antes de aprovar componentes. Status têm texto/ícone além de cor. Foco visível e contraste WCAG AA nas telas finais.

Fonte sans local (Inter como candidata), mono local para issue keys e diagnósticos. Corpo 14 px no desktop, controles touch adequados no mobile. Escala de espaçamento 4/8/12/16/24/32; radius 6/8/12. Sombras apenas para camadas. Motion curto e respeitando prefers-reduced-motion.

### 14.3 Componentes

Button, IconButton, Input, Select, Combobox, DatePicker, Checkbox, Badge, Avatar, Tooltip, Dialog, Drawer, Dropdown, Tabs, Table, Pagination, Toast, InlineAlert, Skeleton, EmptyState, FieldRenderer, IssueCard, StatusColumn, RuleBuilder e SecretInput. Estados default/hover/focus/disabled/loading/error devem ser especificados e verificados.

Sem pacote de componentes que determine sozinho a identidade. Usar primitives acessíveis mantidas quando úteis. Formulários com labels, mensagens associadas e validação preservando valores. Não depender apenas de toast para erro de formulário.

### 14.4 Navegação e telas

- Sidebar: organização, meus itens, projetos, busca, configurações e administração conforme permissão.
- Projeto: Kanban, lista e configurações; filtros no topo, ação criar sempre acessível.
- Card: tipo/ícone, chave, título, prioridade, responsável e prazo; até dois campos extras escolhidos pelo manager.
- Issue: drawer lateral com URL própria; abrir página inteira quando desejado. Detalhes, campos, comentários e atividade.
- Mobile: issue em tela inteira, colunas navegáveis, menu de mover e layout de lista; drag não é obrigatório.
- Admin separado visualmente pela navegação, usando os mesmos componentes.
- Atalhos: comando global Ctrl/Cmd+K, criar C, busca / e Esc; nunca capturar atalhos enquanto digita em campos.
- Rascunhos de descrição preservados localmente com aviso de dispositivo compartilhado e limpeza no logout. Não persistir conteúdos de outros usuários após troca de sessão.
- Falha de rede mostra estado e preserva edição; sem prometer escrita offline no v1.

PT-BR inicial; arquitetura i18n com inglês previsto, locale para datas/números. Não misturar horário da organização com idioma do usuário.

## 15. Deploy, CLI e operação

Comandos planejados: `puxiri serve`, `puxiri doctor`, `puxiri backup create`, `puxiri backup verify`, `puxiri admin recover` e `puxiri mcp stdio`. Restore offline usa fluxo documentado, não bypass de autorização.

Container não root, filesystem raiz read only quando possível, volume `/data`, shutdown gracioso e healthcheck local. TLS no reverse proxy ou terminação dedicada; documentar URL externa e trust proxy. Sem Docker socket montado.

Docker Compose simples com um serviço Puxiri; reverse proxy opcional. Para MinIO de desenvolvimento, perfil separado, sem torná-lo dependência de produção. systemd com usuário dedicado e acesso restrito ao diretório de dados.

Uma imagem contém assets e executável. CI compila Linux amd64/arm64, roda testes, lint e build web; outros alvos depois. SQLite incluído no build quando necessário; “binário único” não significa ausência de bibliotecas de sistema sem validar o target.

Upgrade: ler notas → backup verificado → parar processo → instalar binário → migrations → readiness. Não assumir que trocar para binário antigo desfaz migration. Downgrade via restore compatível. Sem atualização automática de binário pela UI no v1.

## 16. Estratégia de testes e critérios de aceite

Testes de domínio para invariantes; integração com arquivo SQLite real e conexões concorrentes; E2E com Playwright para fluxos de alto valor. Testes de provider usam mocks de contratos e credenciais de sandbox em smoke tests separados.

| Área | Aceite obrigatório |
|---|---|
| Setup | Visitante sem token não assume instância; setup não reabre após concluir |
| Auth | State/token reuse rejeitado; identidade não vinculada por email; reset e logout revogam acesso |
| Providers | Login e vínculo/desvínculo funcionam nos quatro; Apple relay e callback testados |
| MFA | Admin protegido; recovery code não reutiliza; remoção exige confirmação segura |
| Autorização | Matriz testada em REST/MCP/jobs/busca/export; admin operacional não lê issue privada |
| Banimento | Sessões, tokens e grants invalidados; jobs não seguem com acesso antigo |
| Kanban | Move/rank transacional; conflito não perde dados; teclado substitui drag |
| Campos | Tipos/required/opções cross-project rejeitados; filtros corretos |
| Outbox | Crash após commit não perde evento; retry não duplica efeito local |
| Automação | Ciclo limitado; downtime não gera tempestade; permission change interrompe regra |
| Email | Retry/permanent failure, redaction e teste de provider; UI mostra falha |
| CAPTCHA | Token inválido/reusado rejeitado; indisponibilidade não abre fluxo |
| Backup | Escrita concorrente durante snapshot; kill durante upload não cria backup completo falso |
| Restore | Nova instância recupera conteúdo; token antigo não funciona; nenhum efeito externo dispara |
| Criptografia | Chave errada/adulteração rejeitada; rotação preserva restore de backup antigo |
| UI | Claro/escuro, mobile, foco, contraste, loading/error/empty e preservação de draft |
| Operação | Volume persistente, restart, disco cheio, DB busy, encerramento e readiness |

Teste de restauração é obrigatório para release. Testar corrupção controlada, backup interrompido, credencial S3 expirada, permissões sem delete, fila travada, mudança de timezone e segunda instância tentando abrir diretório.

## 17. Plano de implementação por entregas

Não tratar cada fase como um produto final seguro. A v1 completa inclui todos os recursos de segurança e operação aqui acordados.

### Fase 0 — Spikes e contratos

- [ ] Rust/Axum/SQLite + SPA embutida funcionando em binário.
- [ ] Backup online concorrente e compatibilidade SQLx/binding.
- [ ] MCP HTTP com biblioteca escolhida e OAuth/cliente de referência.
- [ ] Contratos Cloudflare/Resend e requisitos Apple/GitLab validados.
- [ ] Registrar versões, decisões e limitações em ADRs.

Saída: esqueleto executável e decisões técnicas comprovadas, sem UI fictícia de integrações.

### Fase 1 — Fundação e identidade

- [ ] Migrations e schema de usuários/organização/settings.
- [ ] Setup protegido e master key/kit de recuperação.
- [ ] Sessões, senha, MFA, recuperação e CSRF.
- [ ] Papéis, membership, convites e banimento.
- [ ] Matriz de autorização e auditoria testada.

Saída: acesso seguro a uma instalação, antes de expor dados reais.

### Fase 2 — Kanban utilizável

- [ ] Projetos, tipos, status, issues e ordenação.
- [ ] Comentários, histórico, concorrência e idempotência.
- [ ] Busca, filtros, lista e drawer.
- [ ] Campos customizados e regras de migração/arquivamento.
- [ ] Design system claro/escuro, teclado e mobile.

Saída: equipe pode organizar trabalho com acesso por projeto.

### Fase 3 — Administração e provedores

- [ ] UI de configuração, draft/test/activate.
- [ ] Resend e Cloudflare, templates e fila.
- [ ] Google, Apple, GitHub e GitLab.
- [ ] Política de cadastro, emails temporários e três CAPTCHAs.
- [ ] Saúde, uso agregado, gestão operacional de entidades.

Saída: instalação configurável sem editar banco ou arquivo de settings.

### Fase 4 — Recuperação operacional

- [ ] Backup manual/agendado para S3, download e retenção.
- [ ] Criptografia, manifesto, checksum e kit.
- [ ] Wizard de restore/recovery, revogação e pausa de efeitos.
- [ ] Ensaio de desastre e documentação de RPO/RTO medidos.

Saída: dados recuperáveis em outra instância com procedimento testado.

### Fase 5 — Integrações MCP

- [ ] API tokens/scopes e OpenAPI.
- [ ] Tools e schemas, idempotência e versionamento.
- [ ] OAuth MCP, consentimento, discovery e stdio cliente.
- [ ] Testar pelo menos um cliente remoto e um stdio de referência.

Saída: agentes operam issues com permissões equivalentes às pessoas.

### Fase 6 — Automações

- [ ] Outbox, scheduler, leases e recuperação.
- [ ] Editor de regras, condições tipadas e preview.
- [ ] Ações, recorrência, controle de ciclos e budgets.
- [ ] Webhooks protegidos e histórico de runs.

Saída: automações sobrevivem a restart e não bypassam permissões.

### Fase 7 — Release

- [ ] Benchmark, perfil de memória e consultas.
- [ ] E2E e revisão de auth/secrets/SSRF/restore.
- [ ] Docker, systemd, binários e CI por arquitetura.
- [ ] Documentação de instalação, operação, providers e atualização.
- [ ] Release candidate com backup e restore aprovados.

Não fixar prazo antes dos spikes: OAuth MCP, Apple e restauração concentram risco técnico. Kanban sozinho é significativamente menor que o escopo completo acordado.

## 18. Instruções para o Codex iniciar

Use este documento como especificação. Comece pela Fase 0 e siga as dependências. Não troque Rust, SQLite ou o modelo de uma organização por instalação sem registrar problema concreto e proposta de mudança.

1. Leia o repositório e instruções locais; registre o estado antes de editar.
2. Crie Cargo package, SPA e comandos de desenvolvimento/build reproduzíveis.
3. Entregue uma fatia executável pequena por vez, com migration, regra de negócio e UI quando aplicável.
4. Não invente integração funcional: validação de configuração precisa chamar o provider ou declarar explicitamente que é só validação local.
5. Consulte docs oficiais para versões, OAuth, Apple, CAPTCHA, S3 e revisão MCP; não implemente drafts sem escolha explícita.
6. Autorize toda operação no servidor; frontend não é boundary de segurança.
7. Escreva testes para os invariantes e riscos descritos, evitando testes que só espelham getters.
8. Nunca logue segredo, token, descrição de issue ou conteúdo de backup.
9. Não adicione serviços distribuídos para resolver problemas não medidos.
10. Ao concluir fase, documente o que funciona, validação executada e pendências reais; não declare v1 completa sem os critérios de aceite.

### 18.1 Entregáveis finais do repositório

README com quickstart; docs de arquitetura e segurança; matriz de acesso; guia de providers; OpenAPI; manual MCP; runbook de backup/restore; migrations; build web embutido; testes; Compose/systemd; CI; changelog e catálogo de componentes.

## 19. Referências oficiais e pontos de validação

Documentação consultada em 02/10/2026. As escolhas de produto neste plano são propostas de implementação; fontes abaixo fundamentam os mecanismos técnicos, não certificam a segurança do sistema.

- SQLite WAL: https://www.sqlite.org/wal.html — concorrência de leitura/escrita, escritor único e limites de filesystem.
- SQLite Online Backup: https://www.sqlite.org/backup.html — snapshot de banco em execução; base do backup online.
- SQLite e rede: https://www.sqlite.org/useovernet.html — manter acesso ao arquivo no host que o armazena.
- Cloudflare Email REST: https://developers.cloudflare.com/email-service/api/send-emails/rest-api/ — envio direto por HTTP, sem binding Worker.
- Cloudflare Email Service: https://developers.cloudflare.com/email-service/ — validar habilitação, beta/plano e requisitos atuais na conta.
- MCP Authorization: https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization — referência versionada; conferir revisão estável e SDK no spike.
- MCP discovery: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/authorization-server-discovery — revisar metadata e compatibilidade com versão escolhida.
- OWASP Authentication: https://cheatsheetseries.owasp.org/cheatsheets/Authentication_Cheat_Sheet.html
- OWASP Sessions: https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html
- OWASP Secrets: https://cheatsheetseries.owasp.org/cheatsheets/Secrets_Management_Cheat_Sheet.html

No início das integrações, acrescentar ADRs com links oficiais e versões exatas para Google, Apple, GitHub, GitLab, Resend e cada CAPTCHA. Não inferir que habilitar CAPTCHA, OAuth ou Rust torna o produto automaticamente seguro: segurança depende do conjunto implementado, dos testes e da operação.



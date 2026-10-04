# Puxiri — Plano completo de desenvolvimento

Versão: 1.1 · Atualizado: 03/10/2026 · Status: decisões de produto consolidadas; validação técnica pela Fase 0 ainda pendente.

Esta revisão incorpora as decisões da sessão de definição de produto nas seções correspondentes, substituindo regras anteriores incompatíveis. Recursos acordados não estão implementados nem tecnicamente comprovados apenas por constarem neste plano.

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
| Emails | Amazon SES, Resend, SendGrid e Cloudflare Email Service; um provider ativo |
| CAPTCHA | Turnstile, reCAPTCHA ou hCaptcha |
| Login externo | Google, Apple, GitHub e GitLab |
| Arquivos e backup remoto | S3 compatível privado; prefixos separados; URLs assinadas para transferências do usuário |
| MCP | Transporte HTTP e opção stdio pelo mesmo binário |

Nome escolhido pelo usuário. Registro de marca, domínio e disponibilidade de namespaces não estão confirmados. Isso não impede trabalhar no repositório genérico.

Repositório fechado, direitos reservados e sem licença pública até decisão anterior à primeira distribuição pública. Não apresentar como open source. Essa decisão não bloqueia commits nem desenvolvimento privado.

### 1.1 Escopo da primeira versão completa

- Setup e restauração de nova instância.
- Organização, membros, convidados, convites e projetos.
- Kanban, issues, tipos, status, labels por projeto, responsável, prioridade e data de entrega.
- Campos customizados tipados, busca, filtros, comentários e histórico.
- Autenticação, autorização, sessões, MFA administrativo, bloqueio de contas.
- Login por senha configurável e quatro provedores externos.
- Controle de cadastro, emails temporários e CAPTCHA.
- Sino de notificações e emails transacionais com fila persistente, preferências e orçamento.
- Atualização leve por SSE; imagens de issues e avatares opcionais em S3 privado.
- MCP com autorização e trilha de alterações.
- Automações por evento e por agendamento.
- Painel operacional, métricas agregadas, auditoria, backup, exportação e restauração.
- Design system, temas claro/escuro, teclado e acessibilidade.

### 1.2 Fora do escopo inicial

Sprints, story points, roadmap, Gantt, dependências entre issues, cobrança, apps nativos, anexos genéricos, time tracking, fórmulas em campos, scripts arbitrários em automações, integração bidirecional com Git e edição colaborativa de descrição. Imagens e avatares opcionais em S3 são a exceção ao adiamento de anexos. Sem transferência de issues entre projetos, ações comuns em lote, reimportação de exportações, watchers, @menções, presença/cursor ao vivo, cron livre ou backups incrementais no v1. Um tipo “épico” não cria hierarquia automaticamente. Não incluir IA obrigatória nem telemetria externa obrigatória.

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

Custo recorrente: servidor, disco, arquivos e backups remotos e email. Estimativa na UI usa apenas preços informados pelo admin, com premissas explícitas; componentes desconhecidos não são tratados como gratuitos. Sem preços, mostrar volume e projeção, não valores monetários. O software não exige banco gerenciado ou serviço de fila.

Rate limits separados para leitura, escrita e autenticação, comuns à UI/API/MCP, configuráveis pelo installation admin. Automações têm orçamento próprio por projeto. Padrões de tráfego serão definidos após benchmark; respostas indicam quando tentar novamente. Sem cota de quantidade de issues por projeto no v1. Limites já decididos para arquivos e emails constam nas seções 9 e 12.

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
| `src/labels/`, `src/notifications/`, `src/files/` | Labels, sino, uploads, referências e cotas |
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

IDs internos opacos; UUIDv7 como escolha inicial. Datas de instantes em UTC; data de entrega como `YYYY-MM-DD`, sem converter em timestamp. Timezone IANA da organização define vencimento, agendas, dia do orçamento de email e horários exibidos; padrão sugerido `America/Sao_Paulo`, configurável. Sem fuso por usuário no v1. Timestamps de auditoria permanecem UTC. Usuário escolhe pt-BR/en, com fallback no locale da organização, sem alterar o relógio funcional.

| Entidade | Campos/regras centrais |
|---|---|
| organization | Singleton, nome, locale, timezone |
| users | Nome, estado active/banned/disabled, locale opcional, anonymized_at, timestamps; preservar autoria |
| user_emails | Vários por usuário, um principal confirmado; endereço único entre contas, verificação |
| auth_identities | User, provider, issuer, subject; unique por identidade externa |
| password_credentials | Hash Argon2id e data de alteração |
| mfa_credentials / recovery_codes | TOTP criptografado e códigos de uso único hashed |
| sessions | Hash do token, usuário, expiração, atividade, revogação |
| one_time_tokens | Hash, finalidade, destinatário, expiração e consumo |
| organization_members | Papel owner/admin/member/guest |
| installation_admins | Papel operacional separado da membership |
| invites / invite_projects | Hash, email, papel org member/guest, projetos/papéis, expiry, consumo e cancelamento |
| projects | Chave estável, nome, descrição, archived_at, next_issue_number, vista padrão |
| project_members | User e papel manager/member/viewer |
| issue_types | Projeto, nome, ícone, cor, archived_at |
| statuses | Projeto, nome, categoria backlog/active/done/canceled, ordem e board_visible |
| issues | Projeto, número, título, descrição, tipo, status, responsável, criador, prioridade, due_date, rank, version e timestamps |
| custom_fields | Projeto, chave imutável, nome, tipo, required, visible_on_card |
| custom_field_options | Campo, ID estável, label, ordem, archived_at |
| labels / issue_labels | Catálogo por projeto, nome, cor da paleta, archived_at e referências |
| issue_field_values | Issue/campo únicos; colunas text/number/date/bool/option com CHECK de tipo |
| issue_multi_options | Issue/campo/opção; integridade referencial |
| comments | Issue, autor, corpo, version, timestamps |
| issue_events | Ator, operação e diff estruturado; textos livres sem conteúdo anterior |
| saved_views / user_project_preferences | Filtros estruturados, busca, usuário/projeto, última vista pessoal |
| api_tokens | Hash, owner, scopes, projetos permitidos, expiry e revoked_at |
| automation_rules / automation_rule_versions | Projeto, versões imutáveis de gatilho/condições/ações, enabled, run_as |
| automation_runs / action_runs | Evento, regra/versão, tentativas, resultado e idempotência |
| outbox_events / jobs | Payload versionado, estado, disponibilidade, lease e tentativas |
| recurring_schedules | Timezone, agenda, próximo horário e política de missed runs |
| notifications / notification_preferences | Sino, leitura, retenção, destinatário/recurso e preferências permitidas |
| notification_deliveries / email_provider_configs | Categoria, destinatário, template, versão do provider, tentativas, expiração |
| email_suppressions / email_daily_budget | Endereço bloqueado por retorno/reclamação e tentativas diárias de atividade |
| storage_destinations / file_objects / file_references | Credenciais versionadas, uso/prefixo, chave imutável, checksum, estado e referências |
| upload_reservations | Reserva de cota, chave, tamanho/tipo esperado, expiração e confirmação |
| export_runs | Filtros, formato, destino privado, estado, expiração e artefato |
| settings / secret_values | Dados tipados, ciphertext, versão da chave |
| backup_runs / backup_objects / backup_file_pins | Modalidade, estados, manifesto, arquivos e proteção contra exclusão |
| backup_verification_runs / operational_alerts | Teste isolado de restore, problema agrupado, lembretes e recuperação |
| audit_events | Operações administrativas sem conteúdo privado |
| usage_daily | Agregados com retenção configurável |
| oauth_clients / oauth_grants | Clientes MCP, consentimentos, scopes e revogação |

Schema efetivo será escrito por migration e revisado. FKs compostas ou validação transacional garantem que tipo, status, campos, opções e responsáveis pertençam ao mesmo projeto. Não confiar em IDs fornecidos pelo cliente.

Sem apagar status com issues: exigir migração para outro status, incluindo lixeira, sem restaurar conteúdo excluído. Categoria de status com issues é imutável; nome, ordem e visibilidade podem mudar. Migração administrativa mostra impacto por estado, gera histórico e atualiza conclusão, mas não dispara automações nem emails de atividade. Antes de excluir, corrigir/desativar regras dependentes, sem remapeamento silencioso.

Tipo ou campo em uso é arquivado; mudança de tipo do campo exige migração explícita. Tipos customizados do v1: texto (até 2.000 caracteres), inteiro, data, booleano, opção única e múltipla escolha. Sem pessoa, URL, texto rico ou valor padrão. Required é igual na UI/API/MCP/automações. Ao ativar, validar issues abertas, não canceladas e fora da lixeira; pendências impedem ativação. Reabrir/restaurar exige preencher os campos obrigatórios atuais na mesma operação. Opção arquivada permanece válida onde usada, sem impedir edição de outro campo, mas não pode ser selecionada novamente. Backup mantém valores arquivados.

## 5. Issues e Kanban

### 5.1 Regras

- Chaves de projeto com 2–6 letras A–Z, únicas e imutáveis. Identificadores `DEV-123` gerados em transação a partir de 1, nunca reutilizados.
- Criador vira único manager. Tipo inicial Tarefa. Status: Backlog e Cancelado fora do board; A fazer (backlog), Em progresso (active) e Concluído (done) visíveis. Issue nasce em A fazer.
- Título obrigatório, limite inicial 240 caracteres. Descrição e comentários em Markdown, limite inicial 64 KB por corpo; subconjunto e imagens conforme seções 5.5 e 12.4.
- Prioridades none/low/medium/high/urgent. Um responsável opcional, project member/manager, inclusive guest member; viewer não. Perda de acesso mantém atribuição marcada como sem acesso, sem novos avisos.
- Um board por projeto; cada coluna visível equivale a um status com board_visible habilitado.
- Categoria done define conclusão. Reabrir limpa `completed_at`; cancelamento tem categoria própria.
- Vencimento: data anterior ao dia atual da organização e categoria diferente de done/canceled. Não mover automaticamente issues atrasadas sem regra configurada.
- Sem permissão por issue no v1: todo conteúdo de uma issue herda acesso do projeto.
- Lixeira: retenção configurável, padrão 30 dias. Member/manager enviam e restauram; viewer não. A leitura existente permanece até purge. Fora do board, busca padrão e automações. Purge automático autorizado/auditado; antecipado apenas pelo manager. Purge tenta excluir arquivos sem outras referências, respeitando proteção de backup.
- Sem transferência entre projetos nem seleção/mutação comum em lote. Migração de status é exceção administrativa.
- Projeto arquivado: somente leitura, fora da sidebar/busca padrão, link direto autorizado continua funcionando e membros permanecem. Manager/owner/admin podem arquivar e reabrir, com auditoria e sem ganhar leitura. Sem exclusão permanente de projeto.
- Arquivar pausa regras/agendas; desarquivar não retoma automaticamente. Manager reativa, sem reproduzir eventos antigos nem compensar agendas do período arquivado.

### 5.2 Ordenação e concorrência

Rank inteiro espaçado por coluna como solução inicial. Inserir entre vizinhos; quando não houver espaço, rebalancear em transação. Cliente envia vizinhos e versão, não rank arbitrário. Aplicar move, rank, version, histórico e outbox atomicamente.

Atualizações usam versão esperada; conflito retorna HTTP 409 com instrução para recarregar. UI mostra conflito e preserva rascunho. Idempotency-Key em criação/mutações suscetíveis a retry, escopo por principal/operação; mesma chave com payload diferente retorna conflito.

Drag and drop otimista com rollback visual em falha. Oferecer mover por menu/teclado. Carregar colunas em lotes, com contagens e “carregar mais”; não renderizar milhares de cards de uma vez. Preservar filtros na URL e scroll ao fechar issue.

### 5.3 Busca e filtros

FTS por título/descrição, chave exata, filtros por projeto, tipo, status, responsável, prioridade, prazo e campos customizados. Paginação por cursor com desempate por ID. Contagens e facetas respeitam acesso. Sem busca em projetos privados a que o usuário não tem acesso.

Comentários não entram no índice. Labels também são filtráveis. Vistas guardam filtros estruturados e texto da busca, sem linguagem de query. Qualquer usuário com acesso, inclusive viewer, pode salvar vista pessoal. Só manager publica/renomeia/remove vista de projeto e define uma como padrão; última vista pessoal usada prevalece. Filtro inválido exige correção antes de executar, sem ampliar resultados silenciosamente. Campo/label arquivado com valores existentes continua filtrável.

### 5.4 Labels, comentários e histórico

Labels pertencem ao projeto, são opcionais e múltiplas por issue; não substituem tipo nem cruzam projetos. Manager cria, renomeia e arquiva; member aplica existentes. Arquivadas saem do seletor, mas permanecem onde usadas. Paleta fixa de oito cores com pares claro/escuro contrastados, sem hex livre. Card mostra até três e `+N`, sem consumir os dois campos extras.

Autor edita/apaga o próprio comentário sem prazo. Manager apaga comentário alheio, mas não reescreve seu texto. Viewer não comenta. Sem lixeira de comentários. Histórico registra edição/remoção sem corpo anterior. Título, descrição e campos customizados de texto também registram apenas que mudaram; campos estruturados mantêm antes/depois. Não preservar textos livres antigos em diffs funcionais.

### 5.5 Markdown, links e atualização ao vivo

CommonMark com autolinks, riscado, tabelas e task lists. Checkboxes são visuais, alterados apenas pela edição do texto. Código monoespaçado, sem highlight. Sem HTML bruto, @menção, imagem remota ou data URI. Imagem remota vira texto do link sanitizado, sem fetch. Sem prefixo de imagens ativo, colar imagem é recusado com explicação; com S3 ativo, usar o fluxo privado da seção 12.4.

Chaves de issue podem virar links/previews somente para quem tem acesso; para os demais, preservar apenas o texto digitado, sem revelar existência/título. Isso não cria dependências.

Board aberto recebe SSE leve com ID, versão e tipo de mudança, sem título/texto. Aviso e nova consulta respeitam acesso atual. Cliente revalida e busca dados da tela. Sem presença/cursor; aba em background desconecta. Queda de SSE usa poll de 30 segundos. Conflitos continuam HTTP 409.

## 6. Autorização e privacidade

Política deny by default. Cada endpoint, ferramenta MCP, job e download verifica principal, estado da conta e acesso atual. Alterar papel invalida caches de acesso; não confiar em claims de longa duração.

Owner tem poderes de org admin e transferência de ownership. Guest é papel de organização, limitado a viewer/member em projetos específicos; nunca manager. Manager exige org member. Na tabela, Member/Viewer são papéis de projeto, inclusive quando exercidos por guest. Papéis operacionais e organizacionais não dão acesso ao conteúdo automaticamente.

| Ação | Installation admin | Org owner/admin | Project manager | Member | Viewer |
|---|---|---|---|---|---|
| Configurar auth/email/CAPTCHA/backup | Sim | Só se também installation admin | Não | Não | Não |
| Saúde e métricas agregadas | Sim | Não | Não | Não | Não |
| Gerenciar membros da organização | Não por esse papel | Sim | Não | Não | Não |
| Criar projeto | Não por esse papel | Sim | Não | Não | Não |
| Consultar conteúdo | Só se membro do projeto | Só se membro do projeto | Sim | Sim | Sim |
| Criar/editar/mover issues e comentar | Só com papel de projeto | Só com papel de projeto | Sim | Sim | Não |
| Campos/status/tipos/automações | Só com papel de projeto | Só com papel de projeto | Sim | Não | Não |
| Gerenciar membros do projeto | Não por esse papel | Sim | Sim, sem elevar a org admin | Não | Não |
| Banir usuário da instalação | Sim | Só se installation admin | Não | Não | Não |
| Exportar conteúdo de projeto | Só se project manager | Só se project manager | Sim | Não | Não |
| Baixar backup completo | Sim + reautenticação/MFA | Só se installation admin | Não | Não | Não |

Matriz fixa no v1, sem settings para liberar criação/exportação a members. Org admin pode controlar membership, nomear manager e arquivar projetos, sem leitura automática. Autoconcessão é auditada. Sidebar só lista projetos acessíveis; member sem projeto vê vazio. Owner/admin têm catálogo operacional com nome/chave/arquivo/contagens, sem issues. Guest não vê diretório da org; no projeto vê nome/papel dos participantes, sem email. Emails são visíveis a manager/owner/admin para gestão.

Saída/remoção voluntária do último owner, installation admin ou manager exige substituto. Usuário sai de projeto sozinho, salvo último manager; autoria permanece. Guest que sai do último projeto mantém conta sem acesso, podendo gerenciar dados e aceitar convites. Projeto é obrigatório na concessão inicial de guest, não para impedir saída.

Banimento de segurança não espera substituto: installation admin pode banir último manager, tornando o projeto somente leitura até owner/admin nomear outro. Outro installation admin pode banir último owner, deixando gestão da organização bloqueada até recuperação auditada. Bloquear último installation admin exige acesso local ao host e recuperação. Novo installation admin é nomeado apenas por um existente, precisa ser org member com TOTP ativo e não ganha leitura de projeto.

Painel operacional lista identidades necessárias à gestão, estado, papéis, contagens e consumo. Não exibe títulos, textos, comentários, resultados de busca, tokens nem payloads de automações. Sem SQL console ou impersonação no v1.

Backups completos contêm conteúdo privado. Criptografia protege armazenamento/transporte, mas não impede o administrador autorizado a restaurá-los. Quem controla host, chave ou memória do processo pode acessar dados. Não apresentar o produto como ponta a ponta.

Banimento revoga sessões, tokens, grants MCP e impede login futuro por qualquer identidade. Interromper novas execuções sob aquele usuário; jobs revalidam acesso ao executar. Permitir desfazer banimento, sem ressuscitar tokens antigos. Exigir motivo e auditar.

Usuário desativa a própria conta, revogando acesso; admin pode reativar, sem ressuscitar tokens. Anonimização é ação distinta de installation admin ou owner: substituir identidade por Usuário removido e remover emails, avatar, identidades externas, senha, MFA, sessões, tokens e grants pessoais, preservando trabalho e referências de autoria. Últimos responsáveis precisam de substituição. Conteúdo livre pode conter dados pessoais: essa operação, sozinha, não garante anonimização efetiva nem conformidade com LGPD. Prever procedimento de avaliação de pedidos e retenção legal. Backups antigos seguem retenção; antes de liberar restore, revisar pedidos de exclusão posteriores ao snapshot, sem prometer apagamento imediato de todas as cópias.

Auditoria separada: owner/admin veem membros, convites, papéis e autoconcessão; installation admin vê settings/auth/backup/banimento/recuperação. Manager não ganha log administrativo. Nenhuma dessas trilhas contém texto/título de issue, comentário ou token. Histórico funcional continua autorizado pelo projeto.

## 7. Autenticação, cadastro e recuperação

### 7.1 Setup seguro

Instância vazia inicia em modo setup. Gerar token aleatório de uso único no console local; nunca permitir que o primeiro visitante público se torne administrador. Wizard exige esse token. Rota setup deixa de existir após finalização.

Wizard: criar organização e administrador → criar kit de recuperação → configurar URL canônica e timezone → email → método de login → política de cadastro/CAPTCHA → backup → testar → finalizar. Provedores externos começam desativados. Cadastro padrão: convite. MFA TOTP obrigatório para installation admins após bootstrap; códigos de recuperação de uso único.

Usuário do wizard é org owner e installation admin, com endereço estabelecido pelo bootstrap local protegido; não entra automaticamente em projetos futuros. Email e S3 são recomendados, não obrigatórios. Wizard explica sem jargão: email permite convites, verificação/reset e avisos; S3 permite backup fora do servidor, imagens, avatares e exportações. São configurações independentes. Sem email, gestor copia links de convite e recuperação administrativa usa acesso local. Sem S3, recursos dependentes ficam indisponíveis; alerta explica que falha do servidor pode causar perda definitiva sem backup externo.

MFA é opcional para outros usuários, com exigência configurável separadamente para owner/admin e project managers, desligada inicialmente. Ao exigir, usuário sem MFA precisa configurá-lo antes de exercer poderes administrativos. Installation admin continua obrigatório.

### 7.2 Sessões e senha

- Sessões opacas aleatórias com pelo menos 256 bits; armazenar apenas hash do token.
- Cookie Secure e HttpOnly, SameSite=Lax no fluxo comum; política específica para callback Apple por POST deve ser implementada e testada sem enfraquecer todo o site.
- Rotacionar ID após login e elevação. Expiração inicial: 7 dias absolutos e 24 horas de inatividade, configuráveis. Atualizar atividade em intervalos para reduzir escrita.
- Sessão administrativa privilegiada: reautenticação/MFA recente, janela inicial 10 minutos.
- Senha com Argon2id calibrado no hardware alvo, 10–128 caracteres, passphrases e sem composição obrigatória. Lista local bloqueia senhas óbvias; sem consulta externa de vazamento. Limites contra abuso de custo computacional.
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

Modos iguais para senha/login externo: invite_only é padrão, sem criar conta de pessoa nova sem convite; email confirmado correspondente permite consumir convite. disabled congela emissão e aceitação de convites, sem impedir login de contas existentes. open cria conta em espera, sem papel/projeto/diretório/catálogo; owner/admin promove para member ou guest com projeto. Conta em espera precisa verificar email antes de promoção. Provider só comprova email quando o contrato confirmar isso; senão, verificar localmente antes de consumir convite ou sair da espera.

Convites: email + papel org member/guest + projetos/papéis. Guest novo exige projeto; member pode entrar sem projeto. Owner/admin emitem ambos; manager só convida guest novo a projetos que gerencia, no máximo member, e adiciona member interno existente. Nunca concedem owner/admin. Validade inicial 7 dias, configurável; reenvio invalida link anterior, cancelamento imediato, consumo único. UI lista pendente/aceito/expirado/cancelado. Convite aceito comprova o endereço sem segundo email. Usuário autenticado pode aceitar na conta atual após reautenticação e confirmação do email, se não pertencer a outra conta; sem fusão automática.

Conta pode ter vários emails, um principal confirmado, endereços únicos entre contas; não normalizar pontos/`+` como equivalência. Não remover último endereço. Emails confirmados podem servir para login e correspondência com convite. Avisos vão ao principal; reset vai ao endereço confirmado digitado no fluxo, confirmação ao endereço a confirmar, sem enumeração. Adicionar/remover/trocar principal exige senha novamente ou MFA; sem ambos, novo login pelo provider vinculado, não só sessão aberta. Installation admin usa MFA para alterações sensíveis. Email pendente não recebe reset nem vira principal. Apple relay é endereço válido, não exige email pessoal como substituto.

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

## 9. Notificações e emails transacionais

Adapter comum com `send`, `test_connection`, classificação de erro, provider_message_id e eventos de retorno. Suportar Amazon SES, Resend, SendGrid e Cloudflare Email Service pelas APIs de envio. Cloudflare via REST, sem Worker intermediário; não confundir com Email Routing. Um provider ativo por instalação, sem fallback automático, pois timeout pode ter sido aceito e causar duplicação.

UI: provider, autenticação específica, remetente, nome e reply-to; Cloudflare inclui Account ID, SES região e autenticação AWS conforme contrato validado. Indicar requisitos de domínio, habilitação e limitações da conta. Disponibilidade, quotas e condições comerciais precisam de validação; suporte no software não concede acesso. Não compartilhar automaticamente credenciais de email e S3.

Templates: convite, confirmação, recuperação, alteração sensível, atribuição, comentário, notificação de automação e alerta operacional. HTML/texto simples em pt-BR e inglês no v1. Idioma do destinatário, fallback da org; convite para pessoa sem preferência usa idioma da org. Prévia e teste sem salvar configuração inválida.

Fila no SQLite com backoff/jitter, classificação entre erro temporário/permanente e limite de tentativas. Idempotência quando suportada pelo provider. Em timeout ambíguo, pode haver duplicação; não prometer exactly once externo. Tokens podem ser reemitidos e invalidados sem revelar conteúdo em logs.

Proibir destinatário arbitrário em automação no v1: ações de notificação usam membros autorizados. Emails sobre issues privadas revalidam permissão antes do envio. Alertas de falha de email aparecem na UI mesmo quando o próprio email está quebrado.

### 9.1 Sino e preferências

Inbox in-app é a fonte de avisos: novo responsável; criador/responsável quando outra pessoa comenta; notificações configuradas por automação. Não avisar ator nem duplicar destinatário que acumule papéis. Sem watchers, @menções e aviso automático de mudança de status. Convites/emails de conta são fluxos próprios; alertas operacionais também aparecem no painel e no sino do installation admin.

Sino não é desligável no v1. Abrir marca como lido e vai ao recurso autorizado; opção marcar todos. Contagem até 99+. Lidos duram 30 dias, não lidos 90; expiração não reenvia email. Perda de acesso remove notificações relacionadas e impede leitura. Emails não são cópias obrigatórias do sino.

### 9.2 Orçamento e relevância do email

Email de atividade nasce desligado. Installation admin habilita atribuição, comentário e automação separadamente; usuário pode desligar categorias permitidas, não ligar uma bloqueada. Sem envio retroativo. Regra de automação tem email opcional, subordinado a essas permissões.

Conteúdo de atividade: nome do projeto, chave, título, tipo do aviso e link; sem descrição, comentário ou imagens. Revalidar acesso e relevância antes de cada envio, por exemplo cancelar atribuição que não é mais do destinatário. Expiração de 24 horas, independentemente de retries; sino segue retenção própria.

Teto inicial de 100 tentativas de email de atividade por dia, configurável, no fuso da organização; retries contam. Reservar orçamento atomicamente antes de tentar enviar, considerando timeout ambíguo. Ao atingir teto, novos avisos ficam só no sino, sem acumular envio para amanhã. Retries pendentes aguardam dia seguinte, respeitando expiração/relevância. Convites, verificação, recuperação e alertas operacionais ficam fora desse orçamento, com proteção própria contra abuso.

### 9.3 Troca de provider e retornos

Ao trocar provider, emails nunca tentados usam o novo. Já tentados ficam vinculados à configuração anterior, sem migração automática após timeout. UI mostra pendências antes de remover configuração antiga e preserva dependências necessárias aos retries.

Receber eventos de retorno/reclamação com autenticação e deduplicação conforme provider. Endereço inválido permanente ou reclamação bloqueia novos envios ao endereço, não bane conta. Sino permanece. Falha temporária segue retry. Admin vê diagnóstico redigido, sem conteúdo privado; quotas de email não substituem limites de autenticação e recuperação.

## 10. MCP e API

### 10.1 Contrato comum

REST em `/api/v1`; OpenAPI gerado. Erros JSON com code, message, request_id, field_errors e retryable. Status 401/403/404/409/422/429 usados de forma consistente; acesso a recurso privado desconhecido retorna 404 para não revelar existência.

API tokens criados na UI, mostrados uma vez, armazenados hashed, com nome, validade e scopes por projeto. Scope efetivo é interseção entre token, acesso atual do owner e operação. Nunca utilizar cookies web como autenticação automática de integrações externas.

Guest pode criar tokens conforme papel de projeto; viewer somente leitura. Perda de acesso reduz imediatamente acesso efetivo; banimento revoga tudo. Token pessoal: 90 dias por padrão, prazo menor ou máximo 1 ano, sem nunca expira. UI avisa vencimento; renovar gera novo segredo mostrado uma vez, sem restaurar token revogado.

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

OpenCode, Claude Code, Claude Desktop e Codex precisam funcionar por MCP HTTP remoto no v1, com teste real de conexão/operação/revogação. Stdio é alternativa, não substitui aceite remoto. Fase 0 valida fluxos e requisitos de registro de cada cliente; quando cadastro automático for necessário, usar políticas explícitas e testes, não discovery/registro irrestrito.

Consentimento escolhe leitura/escrita e projetos específicos ou opção explícita Todos os projetos aos quais tenho acesso, inclusive futuros. Esta opção não é marcada implicitamente. Projetos futuros só entram nesse modo; ampliar concessão específica exige consentimento novo. Nenhum grant supera acesso atual da pessoa.

Limites de payload/paginação/concorrência, validação Origin/Host e não passar tokens recebidos para serviços terceiros. Histórico inclui usuário, client/token, source=mcp e request_id.

### 10.3 Famílias de endpoints

`/auth`, `/me`, `/sessions`, `/invites`, `/organization/members`, `/projects`, `/projects/:id/schema`, `/projects/:id/labels`, `/issues`, `/issues/:id/comments`, `/issues/:id/history`, `/views`, `/notifications`, `/files`, `/exports`, `/api-tokens`, `/automations`, `/admin/settings`, `/admin/users`, `/admin/health`, `/admin/usage`, `/admin/backups` e `/mcp`. Contratos exatos seguem casos de uso; endpoints de arquivos emitem URLs/confirmam operações, não fazem proxy dos bytes do usuário.

Fluxo restore isolado em `/setup/restore` somente antes de a instância estar operacional. API e UI não executam SQL arbitrário.

## 11. Automações e scheduler

### 11.1 Configuração

Editor de regras: nome → gatilho → condições → ações → preview → ativar. Condições AND inicialmente; operadores dependem do tipo do campo. JSON versionado com schema de validação; nada de linguagem de programação embutida.

Prévia usa issue escolhida, mostra condições, ações e texto resultante sem mutação/email/webhook. Agendas mostram próximas ocorrências. Criar/comentar/notificar permite variáveis de lista controlada, como chave, título, responsável e prazo, inseridas pelo editor; sem expressões, scripts ou segredos. Destinos respeitam regras próprias: email não recebe descrição/comentário. Required vale nas ações; editor indica regras incompatíveis com schema atual. Todas as ações são restritas ao projeto da regra.

Gatilhos: issue criada, atualizada, status alterado, campo alterado, prazo alcançado, prazo ultrapassado e agenda. Ações: criar issue, mudar status, definir campos, atribuir responsável, comentar, notificar e webhook.

Comparar valores antigos/novos; ação sem mudança real não gera novo evento. Agenda usa timezone IANA e ocorrência única. Política inicial para downtime: consolidar tarefas recorrentes em uma ocorrência pendente, sem criar centenas após restart. UI informa essa política. Em horário inexistente por DST, próxima hora válida; horário repetido executa uma vez por ocorrência local.

Editor de agenda diária/semanal/mensal, com dias/horário no fuso da organização, sem cron livre ou intervalos no v1. Dia mensal inexistente usa último dia do mês. Prazo não gera aviso automaticamente; exige regra ativa, horário configurável e ação. Notificar usa sino por padrão; email opcional sujeito à habilitação administrativa, preferências e teto. Não fixar disparo obrigatório à meia-noite.

### 11.2 Execução confiável

- Persistir outbox na mesma transação da mudança.
- Consumidor agenda jobs com chave única event/rule/version/action.
- Jobs pending/running/succeeded/failed/canceled, lease com vencimento e recuperação após crash.
- Revalidar usuário `run_as` e acesso antes de cada ação. Desativar conta ou perder acesso interrompe regra com erro visível.
- No v1 `run_as` é manager que criou a regra; mudança de owner exige aceitação do novo manager. UI avisa dependência antes de remover usuário.
- Cadeia com correlation_id, depth máximo inicial 5 e limite de execuções por cadeia/projeto.
- Limites iniciais configuráveis: 5 ações por regra e orçamento de execução; retries apenas para falhas transitórias.
- Operações locais e registro de resultado feitos atomicamente. Reexecução não duplica issue/comentário.
- Efeitos externos são at least once; timeout ambíguo e deduplicação devem ser visíveis.
- Histórico de execuções mostra erro redigido e botão de retry autorizado.

Execuções pendentes mantêm versão imutável da regra; edição afeta só eventos novos. Desativar cancela execuções não iniciadas e impede próximas ações de runs em andamento, sem desfazer efeitos concluídos. Arquivamento do projeto também impede novas ações. Evento registra o gatilho, mas condições/ações consultam estado atual; se não atender, não age. Conflito concorrente exige reavaliação antes de retry, sem overwrite silencioso.

Ações seguem ordem configurada. Falha interrompe próximas e deixa resultado parcial, sem rollback de ações já concluídas. Retry retoma ação falha sem repetir concluídas, revalidando condições e permissões. Atomicidade de efeitos locais é por ação e seu resultado, não uma transação aberta envolvendo toda cadeia ou chamadas externas.

### 11.3 Webhooks

HTTPS por padrão, segredo de assinatura HMAC, timestamp e event ID. Credenciais mascaradas. Proibir loopback, metadata cloud e destinos privados por padrão; validar DNS, IP e redirects em cada envio para mitigar SSRF. Destino interno só mediante allowlist específica do installation admin. Limitar tamanho de resposta, timeout e redirecionamentos. Não enviar dados privados fora do projeto sem autorização clara do manager.

Payload padronizado: evento, projeto, chave, título e campos estruturados. Descrição/comentários exigem seleção explícita do manager com aviso de envio externo. Imagens são somente metadados (ID/nome/tipo/tamanho), não bytes nem URL de download; editor usa esse nome para não prometer acesso. Segredos e URLs assinadas nunca entram no payload.

## 12. Arquivos, backups online, exportação e restauração

### 12.1 Configuração S3 na UI

Endpoint, região, bucket, autenticação, path-style/virtual-hosted, timeout e prefixos exclusivos para backup/imagens/avatares/exportações. Usos habilitados independentemente; prefixo vazio mantém recurso indisponível mesmo com backup ativo. Mesmo endpoint pode servir vários usos, sem misturar retenções. Configurações versionadas preservam vínculo dos objetos ao destino de origem. Testar PUT/HEAD/GET/DELETE em prefixo de teste, com cleanup e resultado por capacidade; sem delete, declarar retenção/limpeza como indisponível pelo app. Credenciais restritas aos prefixos dedicados.

Destinos AWS S3, R2, MinIO e outros são candidatos; validar cada um em testes de compatibilidade. TLS validado. MinIO HTTP em rede local somente opção explícita de desenvolvimento. Não configurar ACL pública; bucket deve ser privado.

Transferências de arquivos do usuário somente por URL assinada, nunca por bucket público ou proxy de bytes pelo app. Autorizar emissão por recurso/estado atual; não registrar URLs em logs, auditoria ou Markdown. Configuração que permita acesso anônimo não ativa; spike valida como comprovar privacidade em cada destino, sem tratar um único teste negativo como prova universal. CORS restrito à origem necessária. URL já emitida permanece utilizável até expirar mesmo após revogação: explicar essa janela. Jobs internos de backup/restore/migração/retenção usam credenciais no servidor, sem expor segredos ao browser.

Troca de destino exige migração por padrão. Admin pode optar explicitamente por não migrar: UI explica que downloads antigos dependem das credenciais anteriores e que removê-las perde acesso pela plataforma, sem apagar objetos do bucket. Novos objetos seguem destino novo; não sobrescrever vínculo dos antigos. Desligar novos uploads não impede downloads existentes.

### 12.2 Pipeline de backup

Modalidades: banco (inclui conteúdo/configurações do SQLite, mas não bytes S3) ou completo opcional (banco + imagens e avatares referenciados pelo snapshot). Completo nasce desligado. Cada bundle completo é autossuficiente, sem cadeia incremental; excluir exportações temporárias. UI explica custo/proteção antes de ativar. Não chamar banco-only de garantia de recuperação dos arquivos.

1. Reservar execução única e validar espaço local para snapshot/temporários; falhar com segurança se insuficiente.
2. Criar snapshot via SQLite Online Backup API, por passos com retry/budget, sem copiar arquivo ativo.
3. Fechar snapshot e validar integridade em conexão dedicada.
4. Na modalidade completa, proteger arquivos referenciados pelo snapshot contra exclusão enquanto a execução existir; coletar bytes e validar cada arquivo. Coordenar proteção e snapshot para não deixar uma janela de exclusão; comprovar em teste concorrente. Exclusões aguardam término/cancelamento e revalidam referências.
5. Criar manifesto: modalidade, versão de formato, app/schema, ID da instância, horário UTC, tamanhos, checksums, key ID e inventário dos arquivos. Ausência/inacessibilidade de arquivo torna execução incompleta, com lista e alerta; preservar snapshot, sem declarar sucesso completo ou aplicar retenção nessa execução.
6. Comprimir e criptografar bundle por formato mantido e autenticado, por exemplo age com destinatário público da instalação. Streaming limita consumo de memória. Completo contém os bytes daquela execução.
7. Upload com chave única para o prefixo dedicado; checksum explícito, não tratar ETag como SHA-256 universal.
8. Publicar manifesto de conclusão somente depois de upload completo e verificação disponível no destino.
9. Marcar sucesso; aplicar retenção apenas sobre execuções válidas reconhecidas desta instalação, distinguindo modalidade e preservando último completo válido. Execução incompleta não dispara retenção.
10. Limpar temporários e liberar proteção dos arquivos; nunca apagar dados/backups válidos para abrir espaço. Crash/cancelamento recuperam execuções e proteção sem deixar pins eternos.

Não incluir master key plaintext dentro do backup. Kit de recuperação offline contém chave de configuração e chave privada de recuperação dos bundles, com identificação/versionamento. Chaves de backup separadas dos tokens de acesso S3. Restaurar exige bundle e kit; permitir digitar/importar chaves no wizard por TLS e mantê-las fora de logs.

Retenção inicial sugerida: 7 diários, 4 semanais e 3 mensais, configurável. Agendamento diário; RPO nominal até 24 horas condicionado a execução bem-sucedida. RTO só pode ser estimado após ensaio com tamanho real. Mostrar atraso do último backup válido como alerta.

Backup captura ponto consistente do banco, não inclui binário. UI distingue exportação de projeto de backup criptografado do banco ou completo. Download de backup exige MFA recente e URL assinada de 1 hora, com disposition de anexo; URL não entra em logs. Admin pode emitir outra quando expirar.

Mostrar tamanho estimado antes de ativar completo, real por execução e projeção total conforme retenção. Orçamento de armazenamento de backups configurável, com alertas em 80%/100%, sem bloquear backup nem alterar retenção. Sem orçamento, mostrar volume/projeção sem aviso a cada execução. Custo usa preços informados pelo admin (armazenamento/operações/transferência conhecidos); explicitar premissas/componentes faltantes, sem inventar preços. Email desses alertas é opcional, desligado; painel e sino do installation admin continuam disponíveis.

UI oferece Testar restauração, manual ou agendado: baixar/descriptografar bundle, validar banco/arquivos em staging isolado, sem alterar instalação ou disparar integrações. Mostrar resultado e espaço necessário. Não prometer RPO/RTO ou recuperação dos arquivos só pelo sucesso do upload.

### 12.3 Restauração em nova instância

1. Instância vazia e token local de setup; nenhum cadastro aberto.
2. Escolher bundle em S3 configurado temporariamente ou disponibilizado no host pelo runbook offline, sem persistir credenciais antes de validar. Upload do browser, quando usado, exige S3 temporário e URL assinada; não criar exceção pública/proxy ao contrato de arquivos.
3. Importar material de recuperação, descriptografar para staging, limitar tamanhos e prevenir path traversal/decompression bomb.
4. Validar autenticação criptográfica, checksum, integridade, formato e schema. Recusar backup de versão futura incompatível.
5. Manter original; aplicar migrations no staging somente quando caminho suportado.
6. Para completo, configurar S3 privado e restaurar arquivos do bundle em prefixos novos antes de liberar operação. Admin pode restaurar só banco com aceite de arquivos indisponíveis. Ausência no bundle não usa arquivo do bucket original como substituição silenciosa. Para banco-only, oferecer cópia opcional dos prefixos de imagem/avatar originais, explicando que dependem de ainda existir e podem diferir da data do snapshot; falha avisa e permite seguir com placeholders locais. Sem fetch de imagens externas.
7. Ativar banco com troca atômica e servidor em modo recovery, sem tráfego funcional concorrente.
8. Revogar sessões, invites/reset tokens, tokens API/MCP e OAuth grants. Preservar identidades e conteúdo; exigir login novo e recriação de integrações externas.
9. Pausar regras, agendas, webhooks e entregas pendentes. Não repetir emails e efeitos externos antigos. Jobs de email antigos são cancelados; trabalho local pendente fica para revisão.
10. Reconfigurar URL/callbacks, verificar email/CAPTCHA/destino backup, trocar segredos que possam ter sido expostos.
11. Gerar novo ID operacional e prefixos para não disputar retenção com original. Permitir arquivos restaurados acima da cota, avisando e bloqueando só novos uploads; não descartar nem aumentar limite silenciosamente. Admin pode ajustar antes de liberar.
12. Revisar pedidos de exclusão posteriores ao snapshot e pendências antes de liberar. Registrar origem e recibo sem conteúdo privado.

Não substituir banco de instância ativa pela UI no v1. Recuperação de desastre do mesmo host segue runbook offline, mantendo o original e reutilizando o wizard de recuperação. Testar restore em ambiente isolado sem saída para provedores.

### 12.4 Imagens, avatares e cota

Imagens de descrição/comentário usam prefixo próprio, habilitado separadamente. Member/manager com edição pode colar/enviar; viewer não. Arquivo/lixeira não aceitam upload novo. PNG/JPEG/GIF/WebP até 5 MB, sem anexos genéricos, SVG, imagem remota ou data URI. Guardar ID no corpo, não URL assinada. Referências não cruzam projetos; download exige leitura atual da issue.

Avatar usa terceiro prefixo próprio; sem ele, iniciais e cor estável derivada do ID em paleta acessível. Usuário sobe/remove só a própria foto, admin não define rosto alheio. PNG/JPEG/WebP até 2 MB, sem Gravatar/fetch externo. Download exige visibilidade autorizada da identidade, sem bucket público.

Upload assinado por 5 minutos, para chave específica/tipo/tamanho esperado; download de imagem/avatar por 10 minutos. O mecanismo de restrição real de tamanho depende do contrato S3: reservar cota antes da emissão e validar bytes/tipo/tamanho reais após upload, sem confiar em Content-Type/cliente. Não disponibilizar objeto antes de validação. Spike deve comprovar restrições e impedir sobrescrita tardia de objeto já confirmado com uma URL ainda válida.

Cota conjunta inicial de 1 GB para imagens/avatares, configurável. Considerar objetos e reservas concorrentes atomicamente, mostrando consumo/alerta antes do teto. Ao atingir, bloquear uploads novos, não downloads/backups. Arquivo inválido é apagado; abandonado ou sem referência por 24 horas é limpo e libera cota/reserva. Remover imagem do texto não apaga imediatamente: se ainda referenciada no mesmo projeto, permanece; sem referência aguarda limpeza, respeitando pins de backup. Substituição/remoção de avatar também não deixa objetos órfãos permanentes.

### 12.5 Exportação de projeto

Exportação é job do project manager, exige S3 privado/prefixo exclusivo; sem configuração, UI explica indisponibilidade. JSON inclui schema, issues, campos, labels, comentários/histórico e referências de imagens, sem bytes ou URLs assinadas. CSV é uma linha por issue, campos e labels, sem comentários/histórico. Respeitar filtros escolhidos, excluir lixeira por padrão, sem credenciais nem reimportação no v1.

Revalidar permissão na execução e antes de emitir download assinado de 1 hora. Artefato temporário apagado após 24 horas; nunca servir pelo app como alternativa sem S3. Retenção de backup não toca prefixo de exportação.

## 13. Área administrativa e observabilidade

| Tela | Conteúdo |
|---|---|
| Visão geral | Estado, versão, uptime, alertas e últimas falhas |
| Saúde | DB, disco/WAL, workers, backlog e probes de integração |
| Usuários | Identidade necessária, estado, papéis, sessões e banimento |
| Entidades | Metadados operacionais de organização/projetos; sem leitor de conteúdo |
| Autenticação | Providers, senha, MFA, sessões, cadastro e emails temporários |
| Email e CAPTCHA | Configuração, testes e status |
| Arquivos e S3 | Destinos privados, prefixos, testes, cotas, migração e dependências de credenciais |
| Backups | Modalidade, agenda, retenção, tamanhos/custos, testes, download e restore |
| Uso | Agregados por período, recursos e chamadas |
| Auditoria | Ator, operação, alvo opaco, resultado e horário |

Health público retorna apenas ready/not ready. Diagnóstico detalhado autenticado. `live` indica processo; `ready` indica capacidade de servir o app, banco e migrations. Falha de email ou S3 degrada recurso específico, sem retirar todo Kanban de readiness.

Métricas: latência/erros HTTP por rota, RSS, CPU quando disponível, disco, DB/WAL, busy errors, jobs pendentes/atrasados, duração de automações, backups e chamadas MCP. Sem labels de alta cardinalidade com user ID/issue ID e sem título ou email em métricas. Endpoint Prometheus opcional, autenticado ou restrito à rede local.

Uso: usuários ativos por dia, issues criadas/concluídas, projetos, jobs e chamadas; sem rastrear conteúdo privado ou enviar telemetria externa. Retenção inicial: agregados 12 meses, logs de delivery/jobs 30 dias e auditoria 180 dias, configuráveis. Logs estruturados com request_id e redaction central.

Administrar entidade não é editar tabelas diretamente. Toda operação administrativa passa por caso de uso específico, valida invariantes e registra auditoria.

Alertas operacionais agrupados por problema: notificar surgimento, atualizar alerta existente e no máximo um lembrete diário enquanto persistir; marcar recuperação quando resolver. Painel/sino do installation admin, email opcional desligado por padrão. Email quebrado nunca é o único canal de aviso. Metadados de arquivo/backup e diagnósticos não concedem leitura de conteúdo de projeto.

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

- Sidebar: organização, meus itens, projetos acessíveis, busca, sino, configurações e administração conforme permissão. Conta em espera não vê Meus itens.
- Meus itens: responsável atual = usuário, projetos acessíveis, fora de arquivo/lixeira; não inclui só criação/comentário. Ordenar prazo (sem data no fim), depois atualização mais recente.
- Projeto: Kanban, lista e configurações; filtros no topo, ação criar sempre acessível.
- Card: tipo/ícone, chave, título, prioridade, responsável, prazo, até três labels e +N; até dois campos extras escolhidos pelo manager.
- Issue: drawer lateral com URL própria; abrir página inteira quando desejado. Detalhes, campos, comentários e atividade.
- Mobile: issue em tela inteira, colunas navegáveis, menu de mover e layout de lista; drag não é obrigatório.
- Admin separado visualmente pela navegação, usando os mesmos componentes.
- Atalhos: comando global Ctrl/Cmd+K, criar C, busca / e Esc; nunca capturar atalhos enquanto digita em campos.
- Rascunhos sobrevivem ao recarregamento apenas durante sessão; limpar no logout/expiração/troca de usuário. Perda de acesso detectada limpa rascunhos/telas/caches daquele projeto. Não prometer apagar cópias/capturas salvas fora da plataforma. Senhas/segredos/configuração administrativa não geram rascunhos.
- Falha de rede mostra estado e preserva edição; sem prometer escrita offline no v1.

PT-BR e inglês no v1, preferência do usuário com fallback no locale da organização, inclusive templates de email. Locale formata datas/números; fuso funcional/exibido é da organização, não navegador. Não misturar idioma com horário.

## 15. Deploy, CLI e operação

Comandos planejados: `puxiri serve`, `puxiri doctor`, `puxiri backup create`, `puxiri backup verify`, `puxiri admin recover` e `puxiri mcp stdio`. Restore offline usa fluxo documentado, não bypass de autorização.

Container não root, filesystem raiz read only quando possível, volume `/data`, shutdown gracioso e healthcheck local. TLS no reverse proxy ou terminação dedicada; documentar URL externa e trust proxy. Sem Docker socket montado.

Docker Compose simples com um serviço Puxiri; reverse proxy opcional. Para MinIO de desenvolvimento, perfil separado, sem torná-lo dependência de produção. systemd com usuário dedicado e acesso restrito ao diretório de dados.

Uma imagem contém assets e executável. CI compila Linux amd64/arm64, roda testes, lint e build web; outros alvos depois. SQLite incluído no build quando necessário; “binário único” não significa ausência de bibliotecas de sistema sem validar o target.

Upgrade: ler notas → backup verificado → parar processo → instalar binário → migrations → readiness. Não assumir que trocar para binário antigo desfaz migration. Downgrade via restore compatível. Sem atualização automática de binário pela UI no v1.

## 16. Estratégia de testes e critérios de aceite

Testes de domínio para invariantes; integração com arquivo SQLite real e conexões concorrentes; E2E com Playwright para fluxos de alto valor. S3 usa armazenamento local real privado nas integrações (MinIO em perfil de desenvolvimento), com smoke tests separados para destinos externos. Providers SaaS simulados apenas na fronteira de protocolo, mais credenciais de sandbox em smoke tests separados. Antes de concluir implementação, testes unitários/integração, lint, formatação e build de backend/frontend precisam passar. Não excluir lógica de negócio/autorização/adapters de cobertura para contornar falhas.

| Área | Aceite obrigatório |
|---|---|
| Setup | Token local obrigatório; papéis iniciais corretos; finalizar sem email/S3 com explicações e limitações |
| Auth | State/token reuse rejeitado; sem autovínculo/fusão; vários emails únicos, principal confirmado e reautenticação; reset/logout revogam acesso |
| Providers | Login e vínculo/desvínculo funcionam nos quatro; Apple relay e callback testados |
| MFA | Installation admin obrigatório; exigência configurável para org/project admins bloqueia poderes até adesão; recovery code único |
| Autorização | Guest capped, viewer somente leitura, catálogo/diretório protegidos; matriz REST/MCP/jobs/busca/export/URLs/SSE |
| Banimento | Revoga sessões/tokens/grants; último manager bloqueado imediatamente e projeto read-only; recuperação de últimos responsáveis |
| Kanban | Kit inicial, move/rank transacional, 409/draft, teclado, SSE/poll; sem escrita em arquivo/lixeira |
| Campos | Seis tipos, required igual por canal, reabertura/restore com pendências; opções arquivadas preservadas e cross-project rejeitado |
| Labels e vistas | Catálogo por projeto/paleta; pessoal/projeto e precedência; filtro inválido não amplia resultados |
| Conteúdo | Markdown sem HTML/data URI/fetch remoto; histórico não preserva texto livre anterior; link de issue sem vazamento |
| Lixeira e arquivo | Permissões e leitura preservadas até purge; migração inclui lixeira sem disparos; reabrir projeto não reativa regras |
| Outbox | Crash após commit não perde evento; retry não duplica efeito local |
| Automação | Versão pendente preservada; estado atual, ordem/resultado parcial/retry; cancelamento entre ações; preview sem efeito; só mesmo projeto |
| Email | Quatro providers, sem fallback, pendências na troca, retornos autenticados, bloqueio por endereço e redaction |
| Sino e orçamento | Destinatários/ator, retenção 30/90, permissão/relevância, expiração 24h, 100 tentativas incluindo retry e mudança de dia/fuso |
| CAPTCHA | Token inválido/reusado rejeitado; indisponibilidade não abre fluxo |
| Arquivos | Bucket privado, URL 5/10 min, reserva concorrente de 1 GB, pós-validação/imutabilidade, cross-project rejeitado e limpeza 24h |
| Exportação | Exige S3 e manager; JSON/CSV corretos, sem bytes/segredos; URL 1h e artefato 24h |
| Backup | Banco-only/completo autossuficiente; snapshot concorrente/pins, arquivo ausente = incompleto, sem retenção ao falhar |
| Restore | Bundle completo vs cópia opcional da origem; prefixos novos, cota excedida sem descarte, revogação/pausa e revisão de exclusões |
| Criptografia | Chave errada/adulteração rejeitada; rotação preserva restore de backup antigo |
| MCP | OpenCode/Claude Code/Claude Desktop/Codex remotos; consentimento específico/todos futuros, revogação e stdio alternativo |
| UI | Claro/escuro, mobile, foco/contraste, pt-BR/en, loading/error/empty; drafts limpos em logout/expiração/perda de acesso |
| Operação | Volume persistente, restart, disco cheio, DB busy, encerramento e readiness |
| Alertas e privacidade | Agrupamento/lembrete diário/recuperação, orçamento 80%/100%, custo sem preços fictícios; anonimização e auditorias separadas |

Teste de restauração é obrigatório para release, inclusive completo recuperado sem bucket original. Testar corrupção controlada, backup interrompido, exclusão concorrente de arquivo protegido, credencial S3 expirada, permissões sem delete, fila travada, mudança de timezone, disco cheio e segunda instância tentando abrir diretório. Exercitar Testar restauração pela UI sem efeitos externos. URL assinada expirada/reutilizada e limites reais precisam de testes no destino, não só mocks.

## 17. Plano de implementação por entregas

Não tratar cada fase como um produto final seguro. A v1 completa inclui todos os recursos de segurança e operação aqui acordados.

### Fase 0 — Spikes e contratos

- [x] Rust/Axum/SQLite + SPA embutida funcionando em binário.
- [ ] Backup online concorrente e compatibilidade SQLx/binding; protocolo snapshot/pins e bundle completo com streaming/restauração isolada.
- [ ] MCP HTTP/OAuth com biblioteca mantida e OpenCode, Claude Code, Claude Desktop e Codex; validar discovery/registro necessário por cliente e revogação.
- [ ] Contratos SES/Resend/SendGrid/Cloudflare, autenticação de retornos e requisitos Apple/GitLab validados.
- [ ] S3 privado local real: privacidade, URLs assinadas/CORS, tamanho real/pós-validação/imutabilidade, prefixos, migração e coleta de objetos.
- [ ] Medir recursos do esqueleto e viabilidade em 1 vCPU/1 GB, especialmente Argon2, snapshot e bundle; sem fixar capacidade antes de medir.
- [ ] Registrar versões, decisões e limitações em ADRs.

Saída: esqueleto executável, contratos comprovados e relatório de compatibilidade/limitações, sem UI fictícia de integrações. Não remover requisitos silenciosamente se spike falhar: documentar bloqueio e discutir alternativa.

#### Evidências da primeira fatia da Fase 0

- Esqueleto executável em `src/` e `web/`: Axum 0.8.9, Tokio 1.53.2, SQLx 0.9.0, Svelte 5.57.1 e Vite 8.3.2; dependências resolvidas em `Cargo.lock` e `web/pnpm-lock.yaml`. TypeScript 6.0.3 por compatibilidade com Svelte Check/lint. Sem funcionalidades de login, setup seguro ou Kanban anunciadas como prontas.
- SQLite real: WAL, synchronous FULL, foreign keys e busy timeout conferidos nas quatro conexões; migrations/checksum, persistência, FTS5 e lock entre processos testados. A SPA e as fontes são locais/embutidas; API inexistente não retorna HTML com sucesso.
- `tests/sqlite_backup_spike.rs`: SQLx e rusqlite 0.39.0 compartilham `libsqlite3-sys` 0.37.0. Read transaction fixa snapshot antes dos passos da Online Backup API; 32 transações SQLx confirmam enquanto o snapshot está aberto, e o destino mantém o estado anterior consistente. rusqlite fica apenas em dependências de teste até decisão sobre a integração de produto.
- Backup interrompido pode passar `integrity_check` e ainda estar vazio: publicação exige conclusão da API, inventário/schema/checksums e validações adicionais. Esses testes não comprovam pins, bundle completo, criptografia, retenção ou restauração de produto; a checkbox de backup continua pendente.
- E2E inicia o executável sem Vite, em diretório isolado, no desktop/claro e mobile/escuro; verifica ausência de requisições externas, recuperação de interrupção real de rede e erro de API. `make check` executa format, lint, tipos, testes unitários/integração/E2E e build debug; `make build` gera release.
- Observação local do release fora do repositório, sem Node no PATH: 6.813.632 bytes, RSS 9.052 kB em repouso, 26 threads; assets embutidos, SIGTERM gracioso e integridade após encerramento conferidos. Não é benchmark de capacidade nem validação em 1 vCPU/1 GB; hardware-alvo, Argon2 e custo do bundle continuam pendentes.

Fase 0 continua em andamento: próximos spikes são proteção snapshot/pins/bundle, S3 privado real, contratos dos providers e OAuth/MCP nos quatro clientes. Não expor o esqueleto na internet nem armazenar dados reais.

#### Evidências da segunda fatia — protocolo snapshot/pins

- `tests/snapshot_pins_spike.rs` usa SQLite e arquivos locais reais, em diretórios temporários; schema experimental fora das migrations de produto. Não é mock de S3 nem implementação de backup do produto.
- Protocolo demonstrado: adquirir writer transaction `BEGIN IMMEDIATE`; fixar snapshot por leitura em conexão separada; registrar inventário/pins duráveis antes de liberar o writer; manter a mesma read transaction até terminar Online Backup. A cópia de arquivos usa streaming local após o snapshot do banco, ainda sob proteção dos pins. Não reabrir outra conexão para copiar um snapshot mais novo que o inventário.
- Coleta reivindica o objeto como `deleting` em transação, somente sem referências e sem pins; novas referências rejeitam objetos reivindicados. Bytes são apagados depois do commit. O intervalo entre fixar snapshot e registrar pins foi forçado: conexão SQLx concorrente recebe SQLITE_BUSY, não atravessa o writer lock.
- Após commit dos pins, apagar referências atuais não altera o inventário congelado nem permite coleta dos arquivos protegidos. Dois backups simultâneos mantêm proteção até ambos serem terminais; cancelamento antes de selar faz rollback sem pins/lock residual. Worker interrompido após selar deixa pins duráveis, verificados após reabrir o banco, até recuperação explícita marcar incompleto/liberar pins. Isto não é ainda teste de SIGKILL, fencing/leases ou recuperação automática de jobs de produto.
- Exclusão externa de arquivo protegido gera resultado incompleto/lista de ausentes e mantém snapshot de staging para diagnóstico; nunca vira sucesso por haver um SQLite íntegro. Liberação dos pins e transição terminal são atômicas no experimento; não há retenção/publicação implementadas.
- Referências revalidadas: https://www.sqlite.org/isolation.html e https://www.sqlite.org/backup.html. Read transaction longa pode reter WAL; duração, limites de disco/cancelamento e custo da breve writer transaction para inventários grandes ainda precisam de medição.

Mantêm-se pendentes bundle criptografado/autossuficiente, checksums/imutabilidade real, restauração isolada sem origem, S3 privado, falhas de disco/rede e crash de processo. A checkbox conjunta de backup/pins/bundle continua aberta; esta fatia comprova somente o protocolo concorrente em SQLite/arquivos locais.

#### Evidências da terceira fatia — S3 privado real

- `web/scripts/s3-spike.test.mjs` é harness de desenvolvimento, não adaptador de produto: SDK oficial JS S3/presigner 3.1146.0 e Chromium chamam MinIO real. Node continua desnecessário para executar o binário Rust. `make check` inclui a integração S3; `make test-s3` executa só este spike. Docker/imagem/Chromium ausentes falham o gate, sem skip nem mock substituto.
- MinIO `RELEASE.2026-09-22T19-25-18Z`, imagem Chainguard fixada em `sha256:dc8b7ba9bdd982d28af533121e5438239624460432e090a52f81b36e7bd5f7dd`. O pull de `minio/minio:RELEASE.2025-09-07T16-13-09Z` falhou neste ambiente; a imagem Chainguard já estava instalada. Pull/disponibilidade e arquiteturas dessa imagem em CI ainda precisam de validação. Container exclusivo, dados tmpfs 128 MB, porta aleatória só no loopback, credenciais aleatórias, sem bucket/volume/endpoint de outro ambiente; cleanup remove somente o container criado pelo teste.
- Bucket sem policy pública rejeita GET/listagem anônimos; upload/download assinados funcionam nos quatro prefixos. URLs usam upload 300 s, imagem/avatar 600 s e backup/export 3600 s. Assinaturas retrodatadas de upload/download são rejeitadas pelo destino real; chave adulterada também. Isso não valida autorização por usuário/projeto, que ainda não existe.
- PUT comum assinado aceita reutilização e sobrescrita. PUT com `If-None-Match: *` incluído em SignedHeaders dá 200 na primeira criação e 412 na repetição; duas requisições concorrentes têm um vencedor. Remover o header assinado é rejeitado (400 neste MinIO, não presumir 403 universal). SHA-256 assinado rejeita bytes diferentes; bytes originais permanecem no objeto protegido.
- HEAD mede tamanho real, sem confiar na reserva/declaração. Fixture de 16 bytes ultrapassa reserva experimental de 8 bytes e é removida; não é ainda validação de imagem, cota transacional de 1 GB ou enforcement do limite de 5 MB no produto. Apagar o objeto antes da URL expirar permite recriá-lo com a mesma URL, mesmo condicional. Logo, `If-None-Match` sozinho não soluciona abandono/limpeza/reutilização após delete: o protocolo de staging, expiração/tombstone e confirmação precisa prevenir essa janela sem anunciar o objeto novamente.
- Chromium real confirma PUT/GET cross-origin permitido; origem diferente não lê nem faz upload via preflight, e HEAD confirma ausência do upload bloqueado. CORS não é autorização nem revogação: GET fora do navegador com o mesmo bearer ainda funciona. MinIO aplica allowlist global por configuração neste teste; validar CORS específico e políticas dos destinos reais antes de ativação.
- Listagem por prefixo selecionado exclui fixture de outro prefixo. Isto demonstra filtro de inventário, não substitui policy de credencial, migração de destino ou coleta referenciada/pins no produto.
- Referências: https://docs.aws.amazon.com/AmazonS3/latest/userguide/using-presigned-url.html e https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html. Compatibilidade comprovada somente com a versão MinIO testada; Amazon S3/R2/outros precisam de smoke específico antes de alegação de suporte.

Sete contratos S3 reais adicionados, sem integração de produção liberada. A checkbox S3 da Fase 0 permanece aberta: falta validar pós-confirmação/imutabilidade/limpeza sem janela de reupload, migração e proteção de pins com armazenamento remoto. Backup bundle/restore isolado, providers e MCP continuam pendentes.

### Fase 1 — Fundação e identidade

- [ ] Migrations, usuários/organização/settings e infraestrutura básica de outbox/jobs/leases necessária aos próximos fluxos.
- [ ] Setup protegido, master key/kit e explicações para leigos; email/S3 opcionais.
- [ ] Sessões, senha/lista local, vários emails/principal/reautenticação, MFA obrigatório/configurável, recuperação e CSRF.
- [ ] Papéis, guest capped, convites/espera, saídas, banimento imediato, desativação/anonimização e recuperação dos últimos responsáveis.
- [ ] Matriz de autorização e auditoria testada.

Saída: acesso seguro a uma instalação, antes de expor dados reais.

### Fase 2 — Kanban utilizável

- [ ] Kit inicial de projetos, tipos/status, issues/rank, labels e migração administrativa sem disparos.
- [ ] Comentários/histórico sem textos antigos, Markdown/links seguros, concorrência e idempotência.
- [ ] Busca sem comentários, vistas pessoal/projeto, lista/drawer e Meus itens.
- [ ] Seis tipos de campo/required, arquivamento, opções, lixeira/purge e projetos read-only.
- [ ] SSE/poll e sino persistente básico, sem depender do email.
- [ ] Design system claro/escuro, pt-BR/en, teclado/mobile, drafts por sessão e limpeza de acesso.

Saída: equipe pode organizar trabalho com acesso por projeto.

### Fase 3 — Administração e provedores

- [ ] UI de configuração, draft/test/activate.
- [ ] SES, Resend, SendGrid e Cloudflare; templates pt-BR/en, fila, mudança de provider e retornos autenticados.
- [ ] Preferências do sino/email, teto diário/tentativas/expiração e relevância, sem retroativo; alertas agrupados.
- [ ] Google, Apple, GitHub e GitLab.
- [ ] Política de cadastro, emails temporários e três CAPTCHAs.
- [ ] Saúde, uso agregado, gestão operacional de entidades.

Saída: instalação configurável sem editar banco ou arquivo de settings.

### Fase 4 — Arquivos privados e exportação

- [ ] Destinos S3 privados/testados e usos independentes com prefixos separados, sem bucket público.
- [ ] Upload/download assinado, reserva de cota/validação/imutabilidade e limpeza de objetos abandonados/sem referência.
- [ ] Imagens nas issues e avatar próprio; limites por tipo/arquivo, cota inicial 1 GB, placeholders e permissões.
- [ ] Migração de destino e aceite explícito de dependência das credenciais antigas quando dispensada.
- [ ] Exportação JSON/CSV por job, manager, filtros, URL 1h/artefato 24h, sem bytes de imagens ou reimportação.

Saída: arquivos e exportações privados, com ciclo de vida/cota/autorização testados. A Fase 5 depende dos objetos e referências desta fase para backup completo.

### Fase 5 — Recuperação operacional

- [ ] Backup banco-only e completo opcional/autossuficiente para S3; proteção de arquivos, incompleto sem retenção e espaço local seguro.
- [ ] Criptografia, inventário/checksums, kit, download assinado/MFA e retenção configurável preservando último válido.
- [ ] Tamanho real/projetado, orçamento 80%/100%, custos só com preços informados e alertas opcionais por email.
- [ ] Wizard de restore/recovery, prefixos novos, exceder cota sem descarte, revogação/pausa e revisão de exclusões posteriores.
- [ ] Testar restauração pela UI, manual/agendado e isolado, completo sem depender da origem.
- [ ] Ensaio de desastre e documentação de RPO/RTO medidos.

Saída: dados recuperáveis em outra instância com procedimento testado.

### Fase 6 — Integrações MCP

- [ ] API tokens/scopes, guest/viewer, validade 90 dias/1 ano e OpenAPI.
- [ ] Tools e schemas, idempotência e versionamento.
- [ ] OAuth MCP, consentimento por projeto ou todos incluindo futuros, discovery/registro controlado e stdio cliente.
- [ ] Aprovar OpenCode, Claude Code, Claude Desktop e Codex via HTTP remoto e stdio alternativo; testar revogação/perda de projeto.

Saída: agentes operam issues com permissões equivalentes às pessoas.

### Fase 7 — Automações

- [ ] Completar outbox/scheduler/leases sobre base da Fase 1, recuperação, versão de regra e cancelamento entre ações.
- [ ] Editor, condições no estado atual, preview sem efeitos e variáveis controladas.
- [ ] Ações sequenciais/retry parcial, mesmo projeto, required, sino/email condicionado, agendas diária/semanal/mensal e prazo configurável.
- [ ] Controle de ciclos/budgets, pausa por arquivo/acesso e retomada explícita sem reproduzir período arquivado.
- [ ] Webhooks seguros, seleção de conteúdo/metadados sem URLs assinadas e histórico de runs.

Saída: automações sobrevivem a restart e não bypassam permissões.

### Fase 8 — Release

- [ ] Benchmark, perfil de memória e consultas.
- [ ] E2E e revisão de auth/secrets/SSRF/restore.
- [ ] Docker, systemd, binários e CI por arquitetura.
- [ ] Documentação de instalação, operação, providers e atualização.
- [ ] Release candidate com backup e restore aprovados.
- [ ] Antes de distribuição pública, decidir licença e condições de publicação; não anunciar open source sem essa decisão.

Não fixar prazo antes dos spikes: OAuth MCP, Apple e restauração concentram risco técnico. Kanban sozinho é significativamente menor que o escopo completo acordado.

Dependências: jobs/outbox mínimos começam na Fase 1; sino na Fase 2; email/alertas na Fase 3; arquivos/exports na Fase 4; backup completo/restore na Fase 5. MCP é comprovado cedo na Fase 0 e integrado ao produto na Fase 6. Automações de produto ficam na Fase 7, sem adiar a infraestrutura necessária às filas anteriores. Checkboxes concluídas exigem evidência de execução, não apenas consolidação de requisitos.

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

README com quickstart e direitos de distribuição corretos; docs de arquitetura/segurança e política de retenção/privacidade; matriz de acesso; guia dos quatro providers de email e login; OpenAPI; manual MCP com quatro clientes remotos; guia de S3 privado/URLs/cotas/migração; runbook de backup banco-only/completo/restore/teste; migrations; build web embutido; testes; Compose/systemd; CI; changelog e catálogo de componentes.

## 19. Referências oficiais e pontos de validação

Referências da revisão original, registrada em 02/10/2026; consolidação de produto em 03/10/2026 não revalida disponibilidade, revisões ou versões desses serviços. Fontes fundamentam mecanismos técnicos, não certificam segurança. Revalidar links/revisões nos spikes antes de fixar dependências.

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

No início das integrações, acrescentar ADRs com links oficiais e versões exatas para Google, Apple, GitHub, GitLab, SES, Resend, SendGrid, Cloudflare Email Service, cada CAPTCHA e destinos S3 testados. Registrar compatibilidade real dos quatro clientes MCP, proteção de bucket/URLs, limites de upload e protocolo de snapshot/pins/restore. Não inferir que habilitar CAPTCHA, OAuth ou Rust torna o produto automaticamente seguro: segurança depende do conjunto implementado, dos testes e da operação.

### 19.1 Pendências de validação técnica, não de escolha de produto

- Binding SQLite/SQLx, snapshot concorrente, lock/migrations e custo de recursos.
- OAuth/discovery/registro controlado necessário para cada cliente MCP remoto.
- APIs/autenticação/retornos dos quatro providers de email e requisitos dos providers de identidade.
- S3 privado: presigning/CORS, validação real dos uploads, URLs não reutilizáveis para sobrescrever objeto confirmado, migração e limpeza.
- Bundle autossuficiente com streaming, pins consistentes com snapshot, tamanho local e restore sem bucket original.
- Estimativas de armazenamento/custo com premissas e componentes desconhecidos visíveis.
- Benchmarks para padrões de rate limit/capacidade e testes de segurança/acessibilidade/desastre.

Falha nesses pontos deve gerar evidência, limitação e proposta de ajuste, não implementação fictícia nem mudança silenciosa do produto acordado.

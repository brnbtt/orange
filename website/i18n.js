(() => {
  const catalogs = {
    en: {
      meta: {
        title: 'ORANGE / Peer-to-peer game streaming',
        description: 'Stream your games directly to friends with Orange for Windows. GPU encoding, game audio, and a native player. Add your squad with personal friend codes.',
        ogTitle: 'ORANGE / Peer-to-peer game streaming',
        ogDescription: 'Share your game and game audio directly with friends. Add your squad, see who\'s streaming, and join from Orange. Windows 10 / 11.',
      },
      nav: {
        skip: 'Skip to content',
        home: 'Orange home',
        language: 'Language',
        app: 'The app',
        friends: 'Add friends',
        download: 'Download',
      },
      hero: {
        eyebrow: 'Windows / Game streaming',
        title: 'Stream games.<br><span>Peer to peer.</span>',
        description: 'Share your game window and game audio. Your GPU encodes the video; your squad watches in a native Windows player.',
        download: 'Download for Windows',
        how: 'See how it works',
        platforms: 'Windows 10 / 11 · 64-bit',
        spec: 'One host encode.<br>A direct connection to each viewer.',
        captureLabel: 'Client / Streaming',
        openShot: 'Open the Orange streaming screenshot at full size',
        alt: 'Orange streaming a demo FPS match with a room code and gaming handles in the viewer list.',
        caption: 'Native app capture. Demo gameplay and player data.<br><span>Open image for full size <span aria-hidden="true">↗</span></span>',
      },
      spec: {
        label: 'Streaming features',
        video: 'Video',
        videoValue: 'H.265 / H.264<span>Hardware encoding</span>',
        transport: 'Transport',
        transportValue: 'Peer to peer<span>No server-side re-encode</span>',
        audio: 'Audio',
        audioValue: 'Your game\'s sound<span>App-scoped window audio</span>',
      },
      workflow: {
        eyebrow: '<span>02</span> / Start streaming',
        title: 'Pick your game.<br>Share the match.',
        intro: 'Install Orange on both PCs. You can join a room by code without signing in, or sign in with Discord to add friends.',
        step1Title: 'Select the game',
        step1Body: 'Open <strong>Start streaming</strong>, choose your quality, then click your game\'s preview. Use <strong>Entire display</strong> if you want to share everything on screen.',
        step1Open: 'Open the game picker screenshot at full size',
        step1Alt: 'Orange\'s source picker with demo FPS, racing, and RPG game windows and quality settings.',
        step1Note: 'Click a preview to start sharing.',
        step2Title: 'Send the code',
        step2Body: 'Send a friend the room code. They copy it to their clipboard, then click <strong>Join with a code</strong> in Orange.',
        step2Open: 'Open the streaming screen screenshot at full size',
        step2Alt: 'A demo FPS match in Orange\'s source preview, with the room code and fictional players watching.',
        step2Note: 'Click the room code to copy it.',
      },
      friends: {
        eyebrow: '<span>03</span> / Friend setup · 1.0',
        title: 'Add your squad.<br>No stream required.',
        intro: 'Both players sign in with Discord. Use a personal friend code to send a request, or a room code to join an active stream.',
        step1Title: 'Send a request',
        step1Body: 'Ask your friend to copy their personal friend code and send it to you. Copy it, click <strong>Add friend</strong>, check the profile, then choose <strong>Send request</strong>.',
        step1Open: 'Open the friend request confirmation screenshot at full size',
        step1Alt: 'Orange showing a demo player\'s profile after Add friend, with Send request and Dismiss actions.',
        step1Note: 'Add friend reads your clipboard.',
        step2Title: 'Accept in Requests',
        step2Body: 'The recipient opens <strong>Requests</strong> and clicks <strong>Accept</strong>. One acceptance adds both players. Incoming requests can be declined; sent requests can be cancelled.',
        step2Open: 'Open the incoming Requests screenshot at full size',
        step2Alt: 'Orange\'s Requests tab showing an incoming request from respawned with Accept and Decline actions.',
        step2Note: 'Incoming and sent requests share one inbox.<br><a class="text-link" href="/screenshots/requests.png" target="_blank" rel="noopener">View the full inbox <span aria-hidden="true">↗</span></a>',
        step3Title: 'Join when they\'re live',
        step3Body: 'Once accepted, you\'re in each other\'s <strong>Friends</strong> list. When your friend streams, click <strong>Join</strong>. You don\'t need to send another room code.',
        step3Open: 'Open the gaming friends list screenshot at full size',
        step3Alt: 'Orange\'s Friends tab with fragbyte streaming and a Join button, while nightshift is offline.',
        step3Note: 'Right-click a friend or use their options menu to manage the friendship.',
        disclosure: 'Native Windows captures of the refreshed interface, with fictional player handles and original demo game scenes. Select any image for the full-size UI.',
      },
      media: {
        eyebrow: '<span>04</span> / Under the hood',
        title: 'Your PC does<br>the streaming.',
        intro: 'The relay helps the PCs connect. Video and audio travel directly between them.',
        pathLabel: 'Media flow: capture, hardware encode, direct connection, native player',
        p1Label: '01 / YOUR PC',
        p1Title: 'Capture',
        p1Body: 'Game, window, or display',
        p2Label: '02 / YOUR GPU',
        p2Title: 'Encode once',
        p2Body: 'H.265 or H.264',
        p3Label: '03 / WEBRTC',
        p3Title: 'Send directly',
        p3Body: 'One stream per viewer',
        p4Label: '04 / THEIR PC',
        p4Title: 'Watch',
        p4Body: 'Native Windows player',
        note1: '<strong>Window audio stays with the window.</strong> Window sharing captures that app\'s audio. Whole-display sharing includes system sound.',
        note2: '<strong>Upload matters.</strong> Each viewer receives a separate stream. More viewers need more upload bandwidth. Networks requiring a TURN relay aren\'t supported yet.',
      },
      faq: {
        eyebrow: '<span>05</span> / Details',
        title: 'Before installing.',
        q1: 'What hardware do I need?',
        a1: 'A 64-bit Windows 10 or Windows 11 PC. Sharing requires a compatible hardware H.265 or H.264 encoder. Keep your graphics drivers current; hardware and Windows version affect feature support.',
        q2: 'Do I need a Discord account?',
        a2: 'You can host and join with a room code without signing in. Both players need Discord sign-in to exchange friend requests and use the synced friends list. Orange is independent of Discord.',
        q3: 'Is a friend code the same as a room code?',
        a3: 'No. A personal friend code identifies a player so you can send them a request. A room code joins an active stream. Use <strong>Add friend</strong> for the first and <strong>Join with a code</strong> for the second.',
        q4: 'Who can join my stream?',
        a4: 'Anyone who has your room code can join and can pass it on. The friends list controls who can discover your stream; it doesn\'t restrict access to people with the code.',
        q5: 'What does the installer include?',
        a5: 'The Windows app and its media runtime. It installs for your user and creates a Start menu shortcut. Download buttons on this site link directly to Orange\'s Azure release storage.',
      },
      download: {
        eyebrow: 'Windows / Latest release',
        title: 'Download Orange.',
        blurb: 'Game streaming. Direct to your friends.',
        button: 'Download for Windows',
        platforms: 'Windows 10 / 11 · 64-bit',
        noscript: 'JavaScript is off. These buttons download version <span data-fallback-version>1.0.0</span>, current when this page was published.',
        versionLine: 'Version {} · Windows 10 / 11 · 64-bit',
        updateUnavailable: 'Update check unavailable. Download {} above, or reload to check again.',
      },
      footer: {
        tagline: 'Peer-to-peer game streaming',
        setup: 'Friend setup',
        download: 'Download',
      },
      notFound: {
        title: 'Page not found — Orange',
        eyebrow: '404 / Page not found',
        heading: 'This page doesn\'t exist.',
        body: 'Return to the website for app information and the latest Windows download.',
        back: 'Back to Orange',
      },
    },
    'pt-BR': {
      meta: {
        title: 'ORANGE / Transmissão de jogos ponto a ponto',
        description: 'Transmita seus jogos direto para amigos com o Orange para Windows. Codificação na GPU, áudio do jogo e player nativo. Adicione o time com códigos de amigo.',
        ogTitle: 'ORANGE / Transmissão de jogos ponto a ponto',
        ogDescription: 'Compartilhe o jogo e o áudio direto com amigos. Adicione o time, veja quem está no ar e entre pelo Orange. Windows 10 / 11.',
      },
      nav: {
        skip: 'Pular para o conteúdo',
        home: 'Página inicial do Orange',
        language: 'Idioma',
        app: 'O app',
        friends: 'Adicionar amigos',
        download: 'Baixar',
      },
      hero: {
        eyebrow: 'Windows / Transmissão de jogos',
        title: 'Transmita jogos.<br><span>Ponto a ponto.</span>',
        description: 'Compartilhe a janela do jogo e o áudio. A GPU codifica o vídeo; o time assiste no player nativo do Windows.',
        download: 'Baixar para Windows',
        how: 'Veja como funciona',
        platforms: 'Windows 10 / 11 · 64 bits',
        spec: 'Uma codificação no host.<br>Uma conexão direta com cada espectador.',
        captureLabel: 'Cliente / Transmitindo',
        openShot: 'Abrir a captura da transmissão do Orange em tamanho real',
        alt: 'Orange transmitindo uma partida FPS de demonstração, com código da sala e nicks na lista de espectadores.',
        caption: 'Captura nativa do app. Gameplay e dados de demonstração.<br><span>Abrir imagem em tamanho real <span aria-hidden="true">↗</span></span>',
      },
      spec: {
        label: 'Recursos da transmissão',
        video: 'Vídeo',
        videoValue: 'H.265 / H.264<span>Codificação por hardware</span>',
        transport: 'Transporte',
        transportValue: 'Ponto a ponto<span>Sem recodificar no servidor</span>',
        audio: 'Áudio',
        audioValue: 'O som do seu jogo<span>Áudio da janela do app</span>',
      },
      workflow: {
        eyebrow: '<span>02</span> / Começar a transmitir',
        title: 'Escolha o jogo.<br>Compartilhe a partida.',
        intro: 'Instale o Orange nos dois PCs. Dá para entrar numa sala por código sem login, ou entrar com Discord para adicionar amigos.',
        step1Title: 'Selecione o jogo',
        step1Body: 'Abra <strong>Começar a transmitir</strong>, escolha a qualidade e clique na prévia do jogo. Use <strong>Tela inteira</strong> se quiser compartilhar tudo na tela.',
        step1Open: 'Abrir a captura do seletor de jogos em tamanho real',
        step1Alt: 'Seletor de fontes do Orange com janelas de FPS, corrida e RPG de demonstração e ajustes de qualidade.',
        step1Note: 'Clique numa prévia para começar a compartilhar.',
        step2Title: 'Envie o código',
        step2Body: 'Envie o código da sala a um amigo. Ele copia para a área de transferência e clica em <strong>Entrar com código</strong> no Orange.',
        step2Open: 'Abrir a captura da tela de transmissão em tamanho real',
        step2Alt: 'Uma partida FPS de demonstração na prévia do Orange, com o código da sala e jogadores fictícios assistindo.',
        step2Note: 'Clique no código da sala para copiá-lo.',
      },
      friends: {
        eyebrow: '<span>03</span> / Amigos · 1.0',
        title: 'Adicione o time.<br>Sem precisar transmitir.',
        intro: 'Os dois entram com Discord. Use um código de amigo para enviar um pedido, ou um código de sala para entrar numa transmissão.',
        step1Title: 'Envie um pedido',
        step1Body: 'Peça ao amigo o código pessoal e envie para você. Copie, clique em <strong>Adicionar amigo</strong>, confira o perfil e escolha <strong>Enviar pedido</strong>.',
        step1Open: 'Abrir a captura da confirmação de pedido em tamanho real',
        step1Alt: 'Orange mostrando o perfil de um jogador de demonstração após Adicionar amigo, com Enviar pedido e Dispensar.',
        step1Note: 'Adicionar amigo lê a área de transferência.',
        step2Title: 'Aceite em Pedidos',
        step2Body: 'Quem recebe abre <strong>Pedidos</strong> e clica em <strong>Aceitar</strong>. Uma aceitação adiciona os dois. Pedidos recebidos podem ser recusados; os enviados, cancelados.',
        step2Open: 'Abrir a captura de Pedidos recebidos em tamanho real',
        step2Alt: 'Aba Pedidos do Orange com um pedido de respawned e ações Aceitar e Recusar.',
        step2Note: 'Pedidos recebidos e enviados ficam na mesma caixa.<br><a class="text-link" href="/screenshots/requests.png" target="_blank" rel="noopener">Ver a caixa completa <span aria-hidden="true">↗</span></a>',
        step3Title: 'Entre quando estiver no ar',
        step3Body: 'Depois de aceito, vocês aparecem na lista de <strong>Amigos</strong>. Quando o amigo transmitir, clique em <strong>Entrar</strong>. Não precisa enviar outro código de sala.',
        step3Open: 'Abrir a captura da lista de amigos em tamanho real',
        step3Alt: 'Aba Amigos do Orange com fragbyte transmitindo e um botão Entrar, enquanto nightshift está offline.',
        step3Note: 'Clique com o botão direito num amigo ou use o menu para gerenciar a amizade.',
        disclosure: 'Capturas nativas do Windows da interface atual, com nicks fictícios e cenas originais de demonstração. Selecione qualquer imagem para a UI em tamanho real.',
      },
      media: {
        eyebrow: '<span>04</span> / Por baixo',
        title: 'Seu PC faz<br>a transmissão.',
        intro: 'O relay ajuda os PCs a se conectarem. Vídeo e áudio viajam direto entre eles.',
        pathLabel: 'Fluxo de mídia: captura, codificação por hardware, conexão direta, player nativo',
        p1Label: '01 / SEU PC',
        p1Title: 'Captura',
        p1Body: 'Jogo, janela ou tela',
        p2Label: '02 / SUA GPU',
        p2Title: 'Codifica uma vez',
        p2Body: 'H.265 ou H.264',
        p3Label: '03 / WEBRTC',
        p3Title: 'Envia direto',
        p3Body: 'Um fluxo por espectador',
        p4Label: '04 / O PC DELES',
        p4Title: 'Assiste',
        p4Body: 'Player nativo do Windows',
        note1: '<strong>O áudio da janela fica com a janela.</strong> Compartilhar uma janela captura o áudio daquele app. Tela inteira inclui o som do sistema.',
        note2: '<strong>O upload importa.</strong> Cada espectador recebe um fluxo separado. Mais espectadores pedem mais banda de upload. Redes que exigem TURN ainda não são suportadas.',
      },
      faq: {
        eyebrow: '<span>05</span> / Detalhes',
        title: 'Antes de instalar.',
        q1: 'Qual hardware eu preciso?',
        a1: 'Um PC Windows 10 ou 11 de 64 bits. Transmitir exige um encoder H.265 ou H.264 de hardware compatível. Mantenha os drivers de vídeo atualizados; hardware e versão do Windows afetam o suporte.',
        q2: 'Preciso de uma conta Discord?',
        a2: 'Dá para hospedar e entrar com um código de sala sem login. Os dois precisam entrar com Discord para trocar pedidos e usar a lista sincronizada. O Orange é independente do Discord.',
        q3: 'Código de amigo é o mesmo que código de sala?',
        a3: 'Não. Um código de amigo identifica um jogador para você enviar um pedido. Um código de sala entra numa transmissão ativa. Use <strong>Adicionar amigo</strong> para o primeiro e <strong>Entrar com código</strong> para o segundo.',
        q4: 'Quem pode entrar na minha transmissão?',
        a4: 'Quem tiver o código da sala pode entrar e pode passar adiante. A lista de amigos controla quem descobre a transmissão; não restringe quem já tem o código.',
        q5: 'O que o instalador inclui?',
        a5: 'O app para Windows e o runtime de mídia. Instala para o seu usuário e cria um atalho no menu Iniciar. Os botões de download deste site apontam direto para o armazenamento Azure do Orange.',
      },
      download: {
        eyebrow: 'Windows / Versão mais recente',
        title: 'Baixe o Orange.',
        blurb: 'Transmissão de jogos. Direto para seus amigos.',
        button: 'Baixar para Windows',
        platforms: 'Windows 10 / 11 · 64 bits',
        noscript: 'O JavaScript está desligado. Estes botões baixam a versão <span data-fallback-version>1.0.0</span>, atual quando esta página foi publicada.',
        versionLine: 'Versão {} · Windows 10 / 11 · 64 bits',
        updateUnavailable: 'Não foi possível verificar atualizações. Baixe {} acima, ou recarregue para tentar de novo.',
      },
      footer: {
        tagline: 'Transmissão de jogos ponto a ponto',
        setup: 'Configurar amigos',
        download: 'Baixar',
      },
      notFound: {
        title: 'Página não encontrada — Orange',
        eyebrow: '404 / Página não encontrada',
        heading: 'Esta página não existe.',
        body: 'Volte ao site para informações do app e o download mais recente para Windows.',
        back: 'Voltar ao Orange',
      },
    },
  };

  const storageKey = 'orange-lang';

  function get(copy, path) {
    return path.split('.').reduce((node, key) => node?.[key], copy);
  }

  function detect() {
    try {
      const saved = localStorage.getItem(storageKey);
      if (saved && catalogs[saved]) return saved;
    } catch {
      // Private mode can throw. Follow the browser language instead.
    }
    const tag = (navigator.language || 'en').toLowerCase();
    return tag.startsWith('pt') ? 'pt-BR' : 'en';
  }

  function apply(locale) {
    const copy = catalogs[locale] || catalogs.en;
    document.documentElement.lang = locale === 'pt-BR' ? 'pt-BR' : 'en';
    if (document.querySelector('[data-i18n="notFound.heading"]') && copy.notFound?.title) {
      document.title = copy.notFound.title;
    } else if (copy.meta?.title) {
      document.title = copy.meta.title;
    }
    const description = document.querySelector('meta[name="description"]');
    if (description && copy.meta?.description) description.content = copy.meta.description;
    const ogTitle = document.querySelector('meta[property="og:title"]');
    if (ogTitle && copy.meta?.ogTitle) ogTitle.content = copy.meta.ogTitle;
    const ogDescription = document.querySelector('meta[property="og:description"]');
    if (ogDescription && copy.meta?.ogDescription) ogDescription.content = copy.meta.ogDescription;

    document.querySelectorAll('[data-i18n]').forEach((el) => {
      const value = get(copy, el.dataset.i18n);
      if (typeof value === 'string') el.textContent = value;
    });
    document.querySelectorAll('[data-i18n-html]').forEach((el) => {
      const value = get(copy, el.dataset.i18nHtml);
      if (typeof value === 'string') el.innerHTML = value;
    });
    document.querySelectorAll('[data-i18n-alt]').forEach((el) => {
      const value = get(copy, el.dataset.i18nAlt);
      if (typeof value === 'string') el.alt = value;
    });
    document.querySelectorAll('[data-i18n-aria]').forEach((el) => {
      const value = get(copy, el.dataset.i18nAria);
      if (typeof value === 'string') el.setAttribute('aria-label', value);
    });
    document.querySelectorAll('[data-set-lang]').forEach((button) => {
      button.setAttribute('aria-pressed', String(button.dataset.setLang === locale));
    });
    window.orangeCopy = copy;
    window.orangeLocale = locale;
  }

  function set(locale) {
    if (!catalogs[locale]) return;
    try {
      localStorage.setItem(storageKey, locale);
    } catch {
      // Ignore quota / private-mode failures; the session still switches.
    }
    apply(locale);
  }

  const locale = detect();
  apply(locale);
  document.querySelectorAll('[data-set-lang]').forEach((button) => {
    button.addEventListener('click', () => set(button.dataset.setLang));
  });

  window.orangeI18n = { catalogs, detect, apply, set, fill(template, value) {
    const parts = String(template).split('{}');
    return parts.length === 1 ? template : `${parts[0]}${value}${parts.slice(1).join('')}`;
  } };
})();

use std::{
    collections::{
	HashMap,
	HashSet,
    },
    sync::{
	Arc,
	Mutex
    },
};

use anyhow::anyhow;
use chrono::prelude::*;
use deadpool_sqlite::Pool;
use lazy_static::lazy_static;
use regex::Regex;
use rusqlite::params;
use serenity::{
    async_trait,
    http::{
	CacheHttp,
	Http
    },
    model::{
	channel::{
	    Channel,
	    Message as SerenityMessage,
	},
	prelude::*,
	gateway::Ready
    },
    prelude::*,
    utils::MessageBuilder,
};
use tokio::sync::{
    broadcast,
    Mutex as AsyncMutex
};
use tokio_stream::{
    wrappers::BroadcastStream,
    StreamExt,
    StreamMap,
};

use crate::{
    Message,
};


const TRANSPORT_NAME: &'static str = "Discord";

const VALID_CHARS: &'static str = "0123456789";

pub(crate) struct Discord {
    transport_id: usize,
    token: String,
    guild: GuildId,
    channels: Arc<HashMap<u64,HandlerChannel>>,
    emojis: Mutex<HashMap<String,Emoji>>,
    threads: Arc<Mutex<HashMap<u64,u64>>>,
    pool: Pool,
    pipo_id: Arc<Mutex<i64>>,
    cache_http: Option<Arc<dyn CacheHttp>>
}

struct Handler {
    real_handler: AsyncMutex<RealHandler>,
}

struct RealHandler {
    transport_id: usize,
    channels: Arc<HashMap<u64,HandlerChannel>>,
    threads: Arc<Mutex<HashMap<u64,u64>>>,
    pins: HashSet<MessageId>,
    pool: Pool,
    pipo_id: Arc<Mutex<i64>>,
}

struct HandlerChannel {
    sender: broadcast::Sender<Message>,
    webhook: Option<u64>
}

impl RealHandler {
    async fn invite_create(&mut self, _ctx: Context,
			   _data: InviteCreateEvent) {
	
    }

    async fn channel_pins_update(&mut self, ctx: Context,
				 pins: ChannelPinsUpdateEvent) {
	let mut thread = None;
	let http = CacheHttp::http(&ctx);
	let channel_id = pins.channel_id;
	let sender = match self.get_sender_and_thread(channel_id, &mut thread)
	    .await {
		Some(sender) => sender,
		None => return
	    };
	let pins = match channel_id.pins(http).await {
	    Ok(pins) => pins,
	    Err(e) => {
		eprintln!("Failed to retrieve pins for channel {:#}: {}",
			  channel_id, e);

		return
	    }
	};
	let new_pins: HashSet<MessageId> = pins.into_iter().map(|m| m.id)
	    .collect();
	let old_pins = self.pins.clone();

	for message in old_pins.difference(&new_pins) {
	    let pipo_id = match self.select_id_from_messages(message).await {
		Ok(id) => id,
		Err(e) => {
		    eprintln!("Couldn't retrieve  pipo_id for MessageId {:#}: \
			       {}", message, e);

		    continue
		}
	    };

	    let message = Message::Pin {
		sender: self.transport_id,
		pipo_id,
		remove: true,
	    };

	    eprintln!("Discord: Removing pin...");

	    if let Err(e) = sender.send(message) {
		eprintln!("Failed to send message: {}", e);
	    }
	}

	for message in new_pins.difference(&old_pins) {
	    let pipo_id = match self.select_id_from_messages(message).await {
		Ok(id) => id,
		Err(e) => {
		    eprintln!("Couldn't retrieve  pipo_id for MessageId {:#}: \
			       {}", message, e);

		    continue
		}
	    };

	    let message = Message::Pin {
		sender: self.transport_id,
		pipo_id,
		remove: false,
	    };

	    eprintln!("Discord: Adding pin...");

	    if let Err(e) = sender.send(message) {
		eprintln!("Failed to send message: {}", e);
	    }
	}

	self.pins = new_pins;
    }

    async fn guild_create(&mut self, ctx: Context, guild: Guild) {
	let http = CacheHttp::http(&ctx);
	let webhooks = match guild.webhooks(http).await {
	    Ok(v) => v,
	    Err(e) => {
		eprintln!("Couldn't retrieve webhooks for guild: {}", e);

		return
	    }
	};

	for webhook in webhooks {
	    let channel_id = webhook.channel_id;
	    if let None = self.channels.get(channel_id.as_u64()) { continue }
	    if let Some(name) = webhook.name {
		let channel_name = match channel_id.to_channel(http).await {
		    Ok(c) => match c {
			Channel::Guild(c) => {
			    c.name
			},
			_ => continue
		    },
		    Err(e) => {
			eprintln!("Couldn't get Channel from ChannelId: {}",
				  e);

			continue
		    }
		};

		if name == format!("PIPO {}", channel_name) {
		    self.channels.get_mut(channel_id.as_u64()).unwrap()
			.webhook = None;
		}
	    }
	}

	let channels: Vec<u64> = self.channels.iter()
	    .filter_map(|(id, channel)| {
		match channel.webhook {
		    Some(_) => None,
		    None => Some(*id)
		}
	    }).collect();
	
	for id in channels.iter() {
	    let channel_name = match ChannelId::from(*id).to_channel(http)
		.await {
		    Ok(c) => match c {
			Channel::Guild(c) => {
			    c.name
			},
			_ => continue
		    },
		    Err(e) => {
			eprintln!("Couldn't get Channel from ChannelId: \
				   {}", e);
			
			continue
		    }
		};
	    
	    // match ChannelId::from(*id)
	    // 	.create_webhook(http, format!("PIPO {}", channel_name))
	    // 	.await {
	    // 	    Ok(wh) => self.channels.lock().unwrap().get(id).unwrap()
	    // 		.webhook = Some(*wh.id.as_u64()),
	    // 	    Err(e) => {
	    // 		eprintln!("Error creating webhook: {}", e);
			
	    // 		continue
	    // 	    }
	    // 	}
	}
	
	
	for thread in guild.threads {
	    eprintln!("Thread: {}", thread);
	    if let Some(channel_id) = thread.category_id {
		let id = channel_id.as_u64();
		// If this is a followed channel...
		if let Some(_) = self.channels.get(id) {
		    // ...insert the thread into threads.
		    let thread_id = *thread.id.as_u64();
		    let channel_id = *id;
		    
		    self.threads.lock().unwrap().insert(thread_id, channel_id);
		}
	    }
	}
	
	eprintln!("Threads: {:?}", self.threads);
    }

    async fn message(&mut self, ctx: Context, msg: SerenityMessage) {
	// Sending a message can fail, due to a network error, an
        // authentication error, or lack of permissions to post in the
        // channel, so log to stdout when some error happens, with a
        // description of it.
	eprintln!("Author: {:#?}", msg.author);
	let http = CacheHttp::http(&ctx);
	if msg.author.bot { return }
	if msg.kind != MessageType::Regular
	    && msg.kind != MessageType::InlineReply { return }
	let channel = match msg.channel_id.to_channel(&ctx).await {
	    Ok(channel) => channel,
	    Err(why) => {
		println!("Error getting channel: {:?}", why);

		return;
	    },
	};
	
	if let Channel::Guild(channel) = channel {
	    let mut thread = None;
	    let channel_id = msg.channel_id;
	    // Check if this message is from a channel or
	    // thread that PIPO is a part of.
	    let sender = match self.get_sender_and_thread(channel_id,
							  &mut thread)
		.await {
		    Some(sender) => sender,
		    None => return
		};
	    let pipo_id
		= match self.insert_into_messages_table(&msg).await {
		    Ok(id) => id,
		    Err(e) => {
			eprintln!("Failed to add message to database: \
				   {}", e);

			return
		    }
		};
	    let mut content = msg.content.clone();
	    
	    lazy_static!{
		static ref RE: Regex
		    = Regex::new(r#"^\\\*(.+)\\?\*$"#).unwrap();
	    }

	    content
		= match self
		.parse_content(&ctx, *msg.guild_id.unwrap().as_u64(),
			       &content).await {
		    Ok(s) => s,
		    Err(e) => {
			eprintln!("Error parsing content: {}", e);
			content
		    }
		};

	    for attachment in msg.attachments.iter() {
		content.insert_str(content.len(),
				   &format!("\n{}",
					    attachment.proxy_url));
	    }

	    let mut attachments = Vec::new();
	    let id = 0;

	    if let Some(reply) = msg.referenced_message {
		let mut fallback = None;
		let pipo_id = match self
		    .select_id_from_messages(reply.as_ref()).await {
			Ok(id) => id,
			Err(e) => {
			    eprintln!("Failed to add message to \
				       database: {}", e);

			    return
			}
		    };
		let nick: String;
		if reply.member.is_some() {
		    match &reply.member.as_ref().unwrap().nick {
			Some(s) => nick = s.clone(),
			None => nick = reply.author.name.clone()
		    }
		}
		else { nick = reply.author.name.clone() }
		let author_name = Some(format!("{} ({})", nick,
					       TRANSPORT_NAME));
		let author_icon = reply.author.avatar_url().map(|s| {
		    s.clone()
		});
		
		if let Ok(msg) = channel.message(http, *reply).await {
		    let ts
			= msg.timestamp.format("%B %e, %Y %l:%M %p");
		    let user = msg.author.name;
		    let content = msg.content;

		    fallback = Some(format!("[{}] {}: {}",
					    ts, user, content));
		}
		attachments.push(crate::Attachment {
		    id,
		    pipo_id: Some(pipo_id),
		    fallback,
		    author_name,
		    author_icon,
		    ..Default::default()
		});

		// id += 1;
	    }
	    
	    let attachments = match attachments.len() {
		0 => None,
		_ => Some(attachments)
	    };
	    
	    let message = if let Some(captures)
		= RE.captures(&content) {
		    content = match captures.get(1) {
			Some(c) => c.as_str(),
			None => "",
		    }.to_string();
		    Message::Action {
			sender: self.transport_id,
			pipo_id,
			transport: TRANSPORT_NAME.to_string(),
			username: msg.author.name.clone(),
			avatar_url: msg.author.avatar_url(),
			thread,
			message: Some(content),
			attachments,
			is_edit: false,
			irc_flag: false
		    }
		}
	    else {
		Message::Text {
		    sender: self.transport_id,
		    pipo_id,
		    transport: TRANSPORT_NAME.to_string(),
		    username: msg.author.name.clone(),
		    avatar_url: msg.author.avatar_url(),
		    thread,
		    message: Some(content),
		    attachments,
		    is_edit: false,
		    irc_flag: false,
		}
	    };

	    if let Err(e) = sender.send(message) {
		eprintln!("Couldn't send message {:#}", e);
	    }
	}
    }

    async fn message_delete(&mut self, ctx: Context, channel_id: ChannelId,
			    message_id: MessageId,
			    _guild_id: Option<GuildId>) {
	let channel = match channel_id.to_channel(&ctx).await {
	    Ok(channel) => channel,
	    Err(why) => {
		println!("Error getting channel: {:?}", why);
		
		return;
	    },
	};

	if let Channel::Guild(_) = channel {
	    let mut thread = None;
	    let sender = match self.get_sender_and_thread(channel_id,
							  &mut thread).await {
		Some(sender) => sender,
		None => return
	    };

	    self.delete_message(message_id, &sender).await;
	}
    }

    async fn message_delete_bulk(&mut self, ctx: Context,
				 channel_id: ChannelId,
				 message_ids: Vec<MessageId>,
				 _guild_id: Option<GuildId>) {
	let channel = match channel_id.to_channel(&ctx).await {
	    Ok(channel) => channel,
	    Err(why) => {
		println!("Error getting channel: {:?}", why);
		
		return;
	    },
	};

	if let Channel::Guild(_) = channel {
	    let mut thread = None;
	    let sender = match self.get_sender_and_thread(channel_id,
							  &mut thread).await {
		Some(sender) => sender,
		None => return
	    };

	    for message_id in message_ids {
		self.delete_message(message_id, &sender).await;
	    }
	}
    }

    async fn message_update(&mut self, ctx: Context, msg: MessageUpdateEvent) {
        // Sending a message can fail, due to a network error, an
        // authentication error, or lack of permissions to post in the
        // channel, so log to stdout when some error happens, with a
        // description of it.
	let author = match msg.author { Some(s) => s, None => return };
	if author.bot { return }
	let channel = match msg.channel_id.to_channel(&ctx).await {
	    Ok(channel) => channel,
	    Err(why) => {
		println!("Error getting channel: {:?}", why);
		
		return;
	    },
	};
	
	if let Channel::Guild(_) = channel {
	    let mut thread = None;
	    let channel_id = msg.channel_id;
	    let sender = match self.get_sender_and_thread(channel_id,
							  &mut thread)
		.await {
		    Some(sender) => sender,
		    None => return
		};
	    let pipo_id
		= match self.select_id_from_messages(msg.id).await {
		    Ok(id) => id,
		    Err(e) => {
			eprintln!("Failed to select id from database: \
				   {}", e);

			return
		    }
		};
	    let mut content = match msg.content {
		Some(s) => s,
		None => return
	    };
	    lazy_static!{
		static ref RE: Regex
		    = Regex::new(r#"^\\\*(.+)\\?\*$"#)
		    .unwrap();
	    }
	    
	    content
		= match self
		.parse_content(&ctx,
			       *msg.guild_id.unwrap().as_u64(),
			       &content).await {
		    Ok(s) => s,
		    Err(e) => {
			eprintln!("Error parsing content: {}",
				  e);
			content
		    }
		};

	    if let Some(attachments) = msg.attachments {
		for attachment in attachments.iter() {
		    content.insert_str(content.len(),
				       &format!("\n{}",
						attachment
						.proxy_url));
		}
	    }
	    
	    let message = if let Some(captures)
		= RE.captures(&content) {
		    content = match captures.get(1) {
			Some(c) => c.as_str(),
			None => "",
		    }.to_string();
		    Message::Action {
			sender: self.transport_id,
			pipo_id,
			transport: TRANSPORT_NAME.to_string(),
			username: author.name.clone(),
			avatar_url: author.avatar_url(),
			thread,
			message: Some(content),
			attachments: None,
			is_edit: true,
			irc_flag: true,
		    }
		}
	    else {
		Message::Text {
		    sender: self.transport_id,
		    pipo_id,
		    transport: TRANSPORT_NAME.to_string(),
		    username: author.name.clone(),
		    avatar_url: author.avatar_url(),
		    thread,
		    message: Some(content),
		    attachments: None,
		    is_edit: true,
		    irc_flag: true,
		}
	    };

	    if let Err(e) = sender.send(message) {
		eprintln!("Couldn't send message {:#}", e);
	    }
	}
    }

    async fn thread_create(&mut self, ctx: Context, thread: GuildChannel) {
	if let Some(channel_id) = thread.category_id {
	    let http = CacheHttp::http(&ctx);
	    // When a new thread is created, check to see if it is
	    // a child of a channel PIPO is in before continuing.
	    if !self.channels.contains_key(channel_id.as_u64()) { return }

	    eprintln!("New Thread: {:?}", thread);

	    let webhook = thread.id.create_webhook(http, thread.name).await
		.ok().map(|wh| *wh.id.as_u64());
	    // let channels = self.channels.lock().unwrap();
	    
	    // channels.insert(*thread.id.as_u64(), HandlerChannel {
	    // 	sender: channels.get(channel_id.as_u64()).unwrap().sender
	    // 	    .clone(),
	    // 	webhook
	    // });
						     
	    // Finally, add the ID's of the thread and its parent to
	    // the thread map and create a new webhook for the thread.
	    let mut threads = self.threads.lock().unwrap();
	    let thread_id = *thread.id.as_u64();
	    let channel_id = *channel_id.as_u64();
	    
	    threads.insert(thread_id, channel_id);
	}
    }

    async fn thread_update(&mut self, _ctx: Context, thread: GuildChannel) {
	eprintln!("Updated Thread: {:?}", thread);

	if thread.thread_metadata.unwrap().archived {
	    
	}
    }

    async fn reaction_add(&mut self, ctx: Context, reaction: Reaction) {
	if reaction.user_id == Some(CacheHttp::http(&ctx).get_current_user()
				    .await.unwrap().id) { return }
	let channel = match reaction.channel_id.to_channel(&ctx).await {
	    Ok(channel) => channel,
	    Err(e) => {
		eprintln!("Error getting channel: {:?}", e);

		return
	    }
	};

	if let Channel::Guild(_) = channel {
	    let mut thread = None;
	    let channel_id = reaction.channel_id;
	    let message_id = reaction.message_id;
	    let sender = match self.get_sender_and_thread(channel_id,
							  &mut thread).await {
		Some(sender) => sender,
		None => return
	    };
	    let pipo_id = match self.select_id_from_messages(message_id)
		.await {
		    Ok(id) => id,
		    Err(e) => {
			eprintln!("Failed to select id from databbase: {}", e);

			return
		    }
		};
	    let mut username = None;
	    let mut avatar_url = None;

	    if let Some(m) = reaction.member {
		if let Some(nick) = m.nick {
		    username = Some(nick);
		}
		if let Some(user) = m.user {
		    if username.is_none() {
			username = Some(user.name.clone())
		    }
		    avatar_url = user.avatar_url();
		}
	    }

	    let emoji = match reaction.emoji {
		ReactionType::Custom {
		    animated: _,
		    id: _,
		    name,
		} => name,
		ReactionType::Unicode(twemoji) => Some(twemoji),
		_ => None
	    };

	    if let Some(emoji) = emoji {
		let message = Message::Reaction {
		    sender: self.transport_id,
		    pipo_id,
		    transport: TRANSPORT_NAME.to_string(),
		    emoji,
		    remove: false,
		    username,
		    avatar_url,
		    thread
		};

		if let Err(e) = sender.send(message) {
		    eprintln!("Couldn't send message {:#}", e);
		}
	    }
	}
    }

    async fn reaction_remove(&mut self, ctx: Context, reaction: Reaction) {
	if reaction.user_id == Some(CacheHttp::http(&ctx).get_current_user()
				    .await.unwrap().id) { return }
	let channel = match reaction.channel_id.to_channel(&ctx).await {
	    Ok(channel) => channel,
	    Err(e) => {
		eprintln!("Error getting channel: {:?}", e);

		return
	    }
	};

	if let Channel::Guild(_) = channel {
	    let mut thread = None;
	    let channel_id = reaction.channel_id;
	    let message_id = reaction.message_id;
	    let sender = match self.get_sender_and_thread(channel_id,
							  &mut thread).await {
		Some(sender) => sender,
		None => return
	    };
	    let pipo_id = match self.select_id_from_messages(message_id)
		.await {
		    Ok(id) => id,
		    Err(e) => {
			eprintln!("Failed to select id from databbase: {}", e);

			return
		    }
		};
	    let mut username = None;
	    let mut avatar_url = None;

	    if let Some(m) = reaction.member {
		if let Some(nick) = m.nick {
		    username = Some(nick);
		}
		if let Some(user) = m.user {
		    if username.is_none() {
			username = Some(user.name.clone())
		    }
		    avatar_url = user.avatar_url();
		}
	    }

	    let emoji = match reaction.emoji {
		ReactionType::Custom {
		    animated: _,
		    id: _,
		    name,
		} => name,
		ReactionType::Unicode(twemoji) => Some(twemoji),
		_ => None
	    };

	    if let Some(emoji) = emoji {
		let message = Message::Reaction {
		    sender: self.transport_id,
		    pipo_id,
		    transport: TRANSPORT_NAME.to_string(),
		    emoji,
		    remove: true,
		    username,
		    avatar_url,
		    thread
		};

		if let Err(e) = sender.send(message) {
		    eprintln!("Couldn't send message {:#}", e);
		}
	    }
	}
    }

    async fn ready(&mut self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
	for gs in ready.guilds {
	    eprintln!("GuildStatus: {:?}", gs);
	    match gs {
		GuildStatus::OnlineGuild(guild) => {
		    for thread in guild.threads {
			eprintln!("Thread: {}", thread);
			if let Some(channel_id) = thread.category_id {
			    let id = channel_id.as_u64();
			    // If this is a followed channel...
			    if let Some(_) = self.channels.get(id) {
				// ...insert the thread into threads.
				let mut threads = self.threads.lock().unwrap();
				let thread_id = *thread.id.as_u64();
				let channel_id = *id;
				
				threads.insert(thread_id, channel_id);
			    }
			}
		    }
		},
		_ => ()
	    }
	}
	eprintln!("Threads: {:?}", self.threads.lock().unwrap())
    }

}

impl RealHandler {
    async fn insert_into_messages_table<T: AsRef<MessageId>>(&self,
							     message_id: T)
	-> anyhow::Result<i64> {
	let conn = self.pool.get().await.unwrap();
	let pipo_id = *self.pipo_id.lock().unwrap();
	let message_id = *message_id.as_ref().as_u64();

	eprintln!("Inserting message_id {} into table at id {}", message_id,
		  pipo_id);
	
	conn.interact(move |conn| -> anyhow::Result<usize> {
	    Ok(conn.execute("INSERT OR REPLACE INTO messages (id, discordid) 
                             VALUES (?1, ?2)",
			    params![pipo_id, message_id])?)
	}).await?;

	let ret = pipo_id;
	let mut pipo_id = self.pipo_id.lock().unwrap();
	*pipo_id += 1;
	if *pipo_id > 40000 { *pipo_id = 0 }
	
	Ok(ret)
    }

    async fn select_id_from_messages<T: AsRef<MessageId>>(&self, message_id: T)
	-> anyhow::Result<i64> {
	let conn = self.pool.get().await.unwrap();
	let message_id = *message_id.as_ref().as_u64();
	
	Ok(conn.interact(move |conn| -> anyhow::Result<i64> {
	    Ok(conn.query_row("SELECT id FROM messages WHERE discordid = ?1",
			    params![message_id], |row| row.get(0))?)
	}).await?)
    }

    async fn get_sender_and_thread(&self, channel_id: ChannelId,
				   thread: &mut Option<(Option<String>,
							Option<u64>)>)
	-> Option<broadcast::Sender<Message>> {
	match self.channels.get(channel_id.as_u64()) {
	    Some(channel) => Some(channel.sender.clone()),
	    None => {
		if let Some(parent_id) = self.threads.lock().unwrap()
		    .get(channel_id.as_u64()) {
			*thread = Some((None, Some(*channel_id.as_u64())));

			return Some(self.channels.get(parent_id).unwrap()
				    .sender.clone())
		}

		return None
	    }
	}
    }

    async fn delete_message(&self, message_id: MessageId,
			    sender: &broadcast::Sender<Message>) {
	let pipo_id = match self.select_id_from_messages(message_id).await {
	    Ok(id) => id,
	    Err(e) => {
		eprintln!("Failed to select id from database: {}", e);
		
		return
	    }
	};
	let message = Message::Delete {
	    sender: self.transport_id,
	    pipo_id,
	    transport: TRANSPORT_NAME.to_string()
	};

	if let Err(e) = sender.send(message) {
	    eprintln!("Couldn't send message {:#}", e);
	}
    }

    pub async fn parse_content(&self,
			       ctx: &Context,
			       guild_id: u64,
			       content: &str)
	-> anyhow::Result<String> {
	let http = CacheHttp::http(&ctx);
	let mut ret = String::new();
	let mut chars = content.chars();

	loop {
	    if let Some(c) = chars.next() {
		if c == '<' {
		    match chars.next() {
			// usernames
			Some('@') => {
			    ret.push('@');
			    
			    if let Some(c) = chars.next() {
				let mut id = String::new();
				let is_nickname = match c {
				    '!' => true,
				    _ => false
				};
				let is_role = match c {
				    '&' => true,
				    _ => false
				};

				if !(is_nickname || is_role) { id.push(c) }

				loop {
				    let c = if let Some(c) = chars.next() { c }
				    else { 
					return Err(anyhow!("Unexpected end of \
							    string."))
				    };
				    if c == '>' { break }
				    if !VALID_CHARS.contains(c) {
					return Err(anyhow!("Invalid character \
							    in id: {}", c))
				    }
				    id.push(c);
				}

				let user = if let Ok(id) = id.parse() {
				    if is_role {
					if let Ok(roles)
					    = http.get_guild_roles(guild_id)
					    .await{
						if let Some(role) =
						    roles.into_iter()
						    .find(|r| {
							r.id == id
						    }) { role.name }
						else { "Unknown".to_string() }
					    } else { "Unknown".to_string() }
				    }
				    else if is_nickname {
					match http.get_member(guild_id, id)
					    .await {
						Ok(m) => {
						    if let Some(n)
							= m.nick { n }
						    else {
							match http
							    .get_user(id)
							    .await {
								Ok(u) =>
								    u.name,
								Err(_) =>
								    "Unknown"
								    .to_string()
							    }
						    }
						},
					    Err(_) => {
						match http.get_user(id).await {
						    Ok(u) => u.name,
						    Err(_) => "Unknown"
							.to_string()
						}
					    }
					}
				    }
				    else {
					match http.get_user(id).await {
					    Ok(u) => u.name,
					    Err(_) => "Unknown".to_string()
					}
				    }
				}
				else { "Unknown".to_string() };

				ret.push_str(&user);
			    }
			},
			Some('#') => {
			    let mut id = String::new();
			    
			    ret.push('#');

			    loop {
				let c = if let Some(c) = chars.next() { c }
				else {
				    return Err(anyhow!("Unexpected end of \
							string."))
				};
				if c == '>' { break }
				if !VALID_CHARS.contains(c) {
				    return Err(anyhow!("Invalid character in\
							id: {}", c))
				}
				id.push(c);
			    }

			    let channel = if let Ok(id) = id.parse() {
				if let Ok(c) = http.get_channel(id).await {
				    match c {
					Channel::Guild(c) => c.name,
					Channel::Private(_) => "Private"
					    .to_string(),
					Channel::Category(c) => c.name,
					_ => "Unknown".to_string()
				    }
				}
				else { "Unknown".to_string() }
			    }
			    else { "Unknown".to_string() };

			    ret.push_str(&channel);
			},
			Some(':') => {
			    let mut name = String::new();
			    let mut id = String::new();

			    ret.push(':');

			    loop {
				let c = if let Some(c) = chars.next() { c }
				else {
				    return Err(anyhow!("Unexpected end of \
							string."))
				};
				if c == ':' { break }
				name.push(c);
			    }

			    loop {
				let c = if let Some(c) = chars.next() { c }
				else {
				    return Err(anyhow!("Unexpected end of \
							string."))
				};
				if c == '>' { break }
				if !VALID_CHARS.contains(c) {
				    return Err(anyhow!("Invalid character in\
							id: {}", c))
				}
				id.push(c);
			    }

			    name = if let Ok(id) = id.parse() {
				if let Ok(e) = http.get_emoji(guild_id, id)
				    .await {
					e.name
				    }
				else { name }
			    }
			    else { name };

			    ret.push_str(&name);
			    ret.push(':');
			},
			Some('a') => {
			    if let Some(c) = chars.next() {
				if c == ':' {
				    let mut name = String::new();
				    let mut id = String::new();

				    ret.push(':');

				    loop {
					let c = if let Some(c) = chars.next() {
					    c
					}
					else {
					    return Err(anyhow!("Unexpected end\
								of stream."))
					};
					if c == ':' { break }
					name.push(c);
				    }

				    loop {
					let c = if let Some(c) = chars.next() {
					    c
					}
					else {
					    return Err(anyhow!("Unexpected end\
								of stream."))
					};
					if c == '>' { break }
					if !VALID_CHARS.contains(c) {
					    return Err(anyhow!("Invalid \
								character in \
								id: {}", c))
					}
					id.push(c);
				    }

				    name = if let Ok(id) = id.parse() {
					if let Ok(e)
					    = http.get_emoji(guild_id, id)
					    .await {
						e.name
					    }
					else { name }
				    }
				    else { name };

				    ret.push_str(&name);
				    ret.push(':');
				}
				else {
				    return Err(anyhow!("Time missing opening \
							':'"));
				}
			    }
			    else {
				return Err(anyhow!("Unexpected end of \
						    string."))
			    }
			},
			Some('t') => {
			    let mut time = String::new();
			    let mut style = String::new();

			    if let Some(c) = chars.next() {
				if c == ':' {
				    loop {
					let mut c = if let Some(c)
					    = chars.next() { c }
					else {
					    return Err(anyhow!("Unexpected end\
								of string."))
					};
					if c == ':' {
					    c = match loop {
						if let Some(c) = chars.next() {
						    if c =='>' {
							break Some(c)
						    }
						    style.push(c);
						}
						else { break None }
					    } {
						Some(c) => c,
						None => break
					    };
					}
					if c == '>' { break }
					if !VALID_CHARS.contains(c) {
					    return Err(anyhow!("Time is not a \
								number"))
					}
					time.push(c);
				    }

				    let dt = Utc.timestamp(time.parse()
							   .unwrap_or_else(
							       |_| 0), 0);

				    let fmt = match style.as_str() {
					// 16:20
					"t" => "%H:%M",
					// 16:20:30
					"T" => "%H:%M:%S",
					// 20/04/2021
					"d" => "%d/%m/%Y",
					// 20 April 2021
					"D" => "%d %B %Y",
					// Thursday, 20 April 2021 16:20
					"F" => "%A, %d %B %Y %H:%M",
					// 2 months ago
					"R" => {
					    let _r = Utc::now() - dt;
					    "%m months ago"
					},
					// 20 April 2021 16:20
					_ => "%d %BB %Y %H:%M"
				    };

				    ret.push_str(&dt.format(fmt).to_string());
				}
				else {
				    return Err(anyhow!("Time missing opening \
							':'"));
				}
			    }
			    else {
				return Err(anyhow!("Unexpected end of \
						    string."))
			    }
			},
			Some(_) => {
			    return Err(anyhow!("Invalid markup tag"))
			},
			None => ()
		    }
		}
		else { ret.push(c) }
	    }
	    else { break }
	}

	Ok(ret)
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn invite_create(&self, ctx: Context, data: InviteCreateEvent) {
	self.real_handler.lock().await.invite_create(ctx, data).await;
    }
    
    async fn channel_create(&self, _ctx: Context, channel: &GuildChannel) {
	eprintln!("New channel: {}", channel);
    }
    
    async fn channel_pins_update(&self, ctx: Context,
				 pins: ChannelPinsUpdateEvent) {
	self.real_handler.lock().await.channel_pins_update(ctx, pins).await;
    }

    async fn channel_update(&self, _ctx: Context, channel: Channel) {
	eprintln!("Channel updated: {}", channel);
    }

    async fn guild_create(&self, ctx: Context, guild: Guild) {
	self.real_handler.lock().await.guild_create(ctx, guild).await;
    }
    
    // Set a handler for the `message` event - so that whenever a new message
    // is received - the closure (or function) passed will be called.
    //
    // Event handlers are dispatched through a threadpool, and so multiple
    // events can be dispatched simultaneously.
    async fn message(&self, ctx: Context, msg: SerenityMessage) {
	self.real_handler.lock().await.message(ctx, msg).await;
    }

    async fn message_delete(&self, ctx: Context, channel_id: ChannelId,
			    message_id: MessageId,
			    guild_id: Option<GuildId>) {
	self.real_handler.lock().await.message_delete(ctx, channel_id,
							 message_id, guild_id)
	    .await;
    }

    async fn message_delete_bulk(&self, ctx: Context, channel_id: ChannelId,
				 message_ids: Vec<MessageId>,
				 guild_id: Option<GuildId>) {
	self.real_handler.lock().await.message_delete_bulk(ctx, channel_id,
							      message_ids,
							      guild_id).await;
    }

    async fn message_update(&self, ctx: Context, msg: MessageUpdateEvent) {
	self.real_handler.lock().await.message_update(ctx, msg).await;
    }
    
    async fn thread_create(&self, ctx: Context, thread: GuildChannel) {
	self.real_handler.lock().await.thread_create(ctx, thread).await;
    }

    async fn thread_update(&self, ctx: Context, thread: GuildChannel) {
	self.real_handler.lock().await.thread_update(ctx, thread).await;
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
	self.real_handler.lock().await.reaction_add(ctx, reaction).await;
    }

    async fn reaction_remove(&self, ctx: Context, reaction: Reaction) {
	self.real_handler.lock().await.reaction_remove(ctx, reaction).await;
    }

    // Set a handler to be called on the `ready` event. This is called when a
    // shard is booted, and a READY payload is sent by Discord. This payload
    // contains data like the current user's guild Ids, current user data,
    // private channels, and more.
    //
    // In this case, just print what the current user's username is.
    async fn ready(&self, ctx: Context, ready: Ready) {
	self.real_handler.lock().await.ready(ctx, ready).await;
    }
}

impl Discord {
    pub async fn new(transport_id: usize,
		     bus_map: &HashMap<String,broadcast::Sender<Message>>,
		     pipo_id: Arc<Mutex<i64>>,
		     pool: Pool,
		     token: String,
		     guild_id: u64,
		     channel_mapping: &HashMap<String,String>)
	-> anyhow::Result<Discord> {
	let channels = Arc::new(channel_mapping.iter()
	    .filter_map(|(channelname, busname)| {
		if let Some(sender) = bus_map.get(busname) {
		    Some((channelname.parse::<u64>().unwrap(),
			  HandlerChannel {
			      sender: sender.clone(),
			      webhook: None
			  }))
		}
		else {
		    eprintln!("No bus named '{}' in configuration file.",
			      busname);
		    None
		}
	    }
	    ).collect());

	Ok(Discord {
	    transport_id,
	    token,
	    guild: GuildId::from(guild_id),
	    channels,
	    emojis: Mutex::new(HashMap::new()),
	    threads: Arc::new(Mutex::new(HashMap::new())),
	    pipo_id,
	    pool,
	    cache_http: None
	})
    }

    async fn update_messages_table<T: AsRef<MessageId>>(&self, pipo_id: i64,
							message_id: T)
	-> anyhow::Result<()> {
	let conn = self.pool.get().await.unwrap();
	let message_id = *message_id.as_ref().as_u64();

	conn.interact(move |conn| -> anyhow::Result<usize> {
		Ok(conn.execute("UPDATE messages SET discordid = ?2
                                 WHERE id = ?1",
				params![pipo_id, message_id])?)
	}).await?;

	Ok(())
    }
    
    async fn select_discordid_from_messages(&self, pipo_id: i64)
	-> anyhow::Result<Option<u64>> {
	let conn = self.pool.get().await.unwrap();
	
	let ret = conn.interact(move |conn| -> anyhow::Result<Option<u64>> {
	    Ok(conn.query_row("SELECT discordid FROM messages WHERE id = ?1",
			    params![pipo_id], |row| row.get(0))?)
	}).await?;

	eprintln!("Found ts {:?} at id {}", ret, pipo_id);

	Ok(ret)
    }

    async fn get_discordid_from_slackid(&self, slack_id: String)
	-> anyhow::Result<Option<u64>> {
	let conn = self.pool.get().await.unwrap();
	let old_slack_id = slack_id.clone();
	
	let ret = conn.interact(move |conn| -> anyhow::Result<Option<u64>> {
	    Ok(conn.query_row("SELECT discordid FROM messages 
                               WHERE slackid = ?1",
			      params![slack_id], |row| row.get(0))?)
	}).await?;

	eprintln!("Found ts {:?} at id {}", ret, old_slack_id);

	Ok(ret)
    }

    async fn get_threadid(&self, channel: ChannelId, slack_ts: Option<String>,
			  message: &Option<String>)
	-> anyhow::Result<ChannelId> {
	let http = self.cache_http.as_ref().unwrap().http();

	    // Check for Slack timestamp and, if found, look for a
	    // corresponding Discord `MessageId` to set `channel` to.
	    // Otherwise, treat this as a non-threaded message by
	    // setting `channel` to the `channel` received by this
	    // method.

	if slack_ts.is_some() {
	    let ts = slack_ts.unwrap();
	    
	    if let Some(id) = self.get_discordid_from_slackid(ts).await? {
		// If a corresponding `MessageId` exists for
		// this timestamp, see if there is already
		// a Discord thread for it stored locally.
		// If not, 
		// create a
		// new Discord thread from the `MessageId`.
		eprintln!("found disid: {}", id);
		if self.threads.lock().unwrap().contains_key(&id) {
		    return Ok(ChannelId::from(id))
		}
		
		let name = match message.clone() {
		    Some(mut s) => {
			if s.len() < 2 {
			    format!("{}!", s)
			}
			else { s.truncate(200); s }
		    },
		    None => String::from("New Thread")
		};
		let ret = channel.create_public_thread(http, id, |ct| {
		    ct.name(name)
			.auto_archive_duration(1440)
			.kind(ChannelType::PublicThread)
		}).await;

		if let Ok(thread) = ret {
		    return Ok(thread.id)
		}
		else {
		    if let Err(SerenityError::Http(e)) = ret {
			if let HttpError::UnsuccessfulRequest(e) = *e {
			    if e.error.code == 160004 {
				return Ok(ChannelId::from(id))
			    }
			}
		    }

		    return Err(anyhow!("I don't know"))
		}
	    }
	}

	return Ok(channel)
    }

    async fn find_emoji<H: AsRef<Http>>(&self,
					http: H,
					emoji: &str)
	-> anyhow::Result<Option<Emoji>> {
	{
	    let emojis = match self.emojis.lock() {
		Ok(mg) => mg,
		Err(e) => return Err(anyhow!("Couldn't acquire lock on Mutex: \
					      {}", e))
	    };

	    if let Some(emoji) = emojis.get(emoji) {
		return Ok(Some(emoji.clone()))
	    }
	}

	{
	    let new_emojis = self.guild.emojis(http).await?;

	    let mut emojis = match self.emojis.lock() {
		Ok(mg) => mg,
		Err(e) => return Err(anyhow!("Couldn't acquire lock on Mutex: \
					      {}", e))
	    };

	    *emojis = new_emojis.into_iter().map(|e| {
		(e.name.clone(), e)
	    }).collect();

	    if let Some(emoji) = emojis.get(emoji) {
		return Ok(Some(emoji.clone()))
	    }
	    else {
		return Ok(None)
	    }
	}
    }

    async fn handle_action_message(&self, channel: ChannelId, pipo_id: i64,
				   transport: String, username: String,
				   message: Option<String>, is_edit: bool)
	-> anyhow::Result<()> {
	let mut content = MessageBuilder::new();
	let http = self.cache_http.as_ref().unwrap().http();
	
    	if let Some(message) = message {
	    content
		.push_bold(username)
		.push_line(format!(" [{}]", transport))
		.push_italic(message)
		.build();

	    if is_edit {
		let message_id = self.select_discordid_from_messages(pipo_id)
		    .await?;
		let message_id = match message_id {
		    Some(id) => id,
		    None => return Err(anyhow!("Could find discordid for \
						id: {}", pipo_id))
		};
		
		channel.edit_message(http,
				     message_id,
				     |m| m.content(content)).await?;

		Ok(())
	    }
	    else {
	    self.update_messages_table(pipo_id,
				       channel.say(http, content).await?)
		    .await
	    }
	}
	else { Err(anyhow!("Message is empty")) }
    }

    async fn handle_delete_message(&self, channel: ChannelId, pipo_id: i64)
	-> anyhow::Result<()> {
	let http = self.cache_http.as_ref().unwrap().http();
	let message_id = self.select_discordid_from_messages(pipo_id).await?;
	
	match message_id {
	    Some(id) => {
		let msg_id = MessageId::from(id);
		
		let id = self.channels.get(channel.as_u64())
		    .and_then(|c| c.webhook);
		
		if let Some(id) = id {
		    if let Ok(wh) = WebhookId::from(id).to_webhook(http)
			.await {
			    return Ok(wh.delete_message(http, msg_id).await?)
			}
		}

		Ok(channel.delete_message(http, msg_id).await?)
	    },
	    None => Err(anyhow!("No message for associated id"))
	}
    }

    async fn handle_pin_message(&self, channel: ChannelId, pipo_id: i64,
				   remove: bool) -> anyhow::Result<()> {
	let http = self.cache_http.as_ref().unwrap().http();
	let message_id = self.select_discordid_from_messages(pipo_id).await?;
	
	match message_id {
	    Some(id) => match remove {
		false => Ok(channel.pin(http, id).await?),
		true => Ok(channel.unpin(http, id).await?)
	    },
	    None => Err(anyhow!("No message for associated id"))
	}
    }

    async fn handle_reaction_message(&self, channel: ChannelId, pipo_id: i64,
				     emoji: String, remove: bool)
	-> anyhow::Result<()> {
	let http = self.cache_http.as_ref().unwrap().http();
	let emoji = match emojis::lookup(&emoji) {
	    Some(e) => ReactionType::Unicode(e.as_str().to_string()),
	    None => match self.find_emoji(http, &emoji).await? {
		Some(e) => ReactionType::from(e),
		None => return Err(anyhow!("Couldn't find emoji"))
	    }
	};
	let message_id = self.select_discordid_from_messages(pipo_id).await?;

	if message_id.is_none() {
	    return Err(anyhow!("No message for associated id"))
	}

	let message_id = message_id.unwrap();

	if !remove {
	    return Ok(channel.create_reaction(http, message_id, emoji).await?)
	}
	else {
	    return Ok(channel.delete_reaction(http, message_id, None, emoji)
		      .await?)
	}
    }

    async fn handle_text_message(&self, channel: ChannelId, pipo_id: i64,
				 transport: String, username: String,
				 avatar_url: Option<String>,
				 thread: Option<(Option<String>, Option<u64>)>,
				 message: Option<String>,
				 attachments: Option<Vec<crate::Attachment>>,
				 is_edit: bool)
	-> anyhow::Result<()> {
	if message.is_none() && attachments.is_none() {
	    return Err(anyhow!("Message has no contents"))
	}
	
	let mut content = MessageBuilder::new();
	let http = self.cache_http.as_ref().unwrap().http();
	let channel = match thread {
	    Some((s, _)) => self.get_threadid(channel, s, &message).await?,
	    None => channel
	};

	if let Some(ref message) = message {
	    content
		.push_line(message);
	}

	if let Some(attachments) = attachments {
	    if message.is_none() {
		content.push_line("Attachment:");
	    }

	    for attachment in attachments {
		if attachment.pipo_id.is_some() {
		    let mut message_id = None;
		    let pipo_id = attachment.pipo_id.unwrap();
		    if let Ok(id)
			= self.select_discordid_from_messages(pipo_id).await {
			    message_id = id;
		    }

		    if let Some(message_id) = message_id {
			if let Ok(message) = channel.message(http, message_id)
			    .await {
				let message = message.reply(http,
							    content.build())
				    .await?;

				return self.update_messages_table(pipo_id,
								  message)
				    .await
			    }
		    }

		    if let Some(fallback) = attachment.fallback {
			content.push_quote_line_safe(fallback);
		    }
		}
	    }
	}
	
	if is_edit {
	    let message_id = self.select_discordid_from_messages(pipo_id)
		.await?;
	    let msgid = match message_id {
		Some(id) => MessageId::from(id),
		None => return Err(anyhow!("Could find discordid for id: {}",
					   pipo_id))
	    };
		    
	    let id = self.channels.get(channel.as_u64())
		    .and_then(|c| c.webhook);
		
	    if let Some(id) = id {
		if let Ok(wh) = WebhookId::from(id).to_webhook(http).await {
		    if let Ok(msg) = wh.edit_message(http, msgid, |f| {
			f.content(content.clone())
		    }).await {
			return self.update_messages_table(pipo_id, msg).await
		    }
		}
	    }

	    let mut msg = MessageBuilder::new();
	    
	    msg.push_bold(username)
		.push_line(format!(" [{}]", transport))
		.push_line(content);

	    channel.edit_message(http, msgid, |m| m.content(msg))
		.await?;

	    Ok(())
	}
	else {
	    let id = self.channels.get(channel.as_u64())
		    .and_then(|c| c.webhook);
		
	    if let Some(id) = id {
		if let Ok(wh) = WebhookId::from(id).to_webhook(http).await {
		    if let Ok(msg) = wh.execute(http, true, |f| {
			let ret = f.content(content.clone())
			    .username(format!("{} ({})", username.clone(),
					      transport.clone()));
			if let Some(url) = avatar_url {
			    ret.avatar_url(url);
			}
			
			ret
		    }).await {
			return self.update_messages_table(pipo_id,
							  msg.unwrap()).await
		    }
		}
	    }

	    let mut msg = MessageBuilder::new();
	    
	    msg.push_bold(username)
		.push_line(format!(" [{}]", transport))
		.push_line(content);

	    self.update_messages_table(pipo_id, channel.say(http, msg)
				       .await?).await
	}
    }
    
    pub async fn connect(&mut self) -> anyhow::Result<()> {
	let mut input_buses = StreamMap::new();

	for (id, channel) in self.channels.iter() {
	    input_buses.insert(*id, BroadcastStream::new(channel.sender
							 .subscribe()));
	}

	let handler = Handler { real_handler: AsyncMutex::new(RealHandler {
	    transport_id: self.transport_id,
	    channels: self.channels.clone(),
	    threads: self.threads.clone(),
	    pins: HashSet::new(),
	    pool: self.pool.clone(),
	    pipo_id: self.pipo_id.clone(),
	})};
	let mut client = Client::builder(self.token.clone())
	    .event_handler(handler).await?;

	self.cache_http = Some(client.cache_and_http.clone());

	tokio::spawn(async move {
	    loop {
		match client.start().await {
		    Ok(_) => (),
		    Err(e) => eprintln!("ERROR WITH THE DISCORD LIONT: {}", e),
		}
	    }
	});

	loop {
	    tokio::select! {
		stream = StreamExt::next(&mut input_buses) => {
		    match stream {
			Some((channel, message)) => {
			    let message = message.unwrap();
			    let channel_id = ChannelId(channel);
			    
			    match message {
				Message::Action {
				    sender,
				    pipo_id,
				    transport,
				    username,
				    avatar_url: _,
				    thread: _,
				    message,
				    attachments: _,
				    is_edit,
				    irc_flag: _,
				}=> {
				    if sender != self.transport_id {
					if let Err(e) = self
					    .handle_action_message(channel_id,
								   pipo_id,
								   transport,
								   username,
								   message,
								   is_edit)
					.await {
					    eprintln!("Error handling \
						       Message::Action: \
						       {}", e);
					}
				    }
				},
				Message::Bot {
				    sender: _,
				    pipo_id: _,
				    transport: _,
				    message: _,
				    attachments: _,
				    is_edit: _,
				} => {
				    continue
				},
				Message::Delete {
				    sender,
				    pipo_id,
				    transport: _,
				} => {
				    if sender != self.transport_id {
					if let Err(e)
					    = self
					    .handle_delete_message(channel_id,
								   pipo_id)
					    .await {
						eprintln!("Error handling \
							   Message::Delete: \
							   {}", e);
					    }
				    }
				},
				Message::Names {
				    sender: _,
				    transport: _,
				    username: _,
				    message: _,
				} => {
				    continue
				},
				Message::Pin {
				    sender,
				    pipo_id,
				    remove,
				} => if sender != self.transport_id {
				    if let Err(e) = self
					.handle_pin_message(channel_id,
							     pipo_id,
							     remove).await {
					    eprintln!("Error handling \
						       Message::Pin: {}", e);
					}
				},
				Message::Reaction {
				    sender,
				    pipo_id,
				    transport: _,
				    emoji,
				    remove,
				    username: _,
				    avatar_url: _,
				    thread: _,
				} => {
				    if sender != self.transport_id {
					if let Err(e) =
					    self
					    .handle_reaction_message(channel_id,
								     pipo_id,
								     emoji,
								     remove)
					    .await {
						eprintln!("Error handling \
							   Message::Reaction: \
							   {}", e);
					    }
				    }
				},
				Message::Text {
				    sender,
				    pipo_id,
				    transport,
				    username,
				    avatar_url,
				    thread,
				    message,
				    attachments,
				    is_edit,
				    irc_flag: _,
				} => {
				    if sender != self.transport_id {
					if let Err(e) = self
					    .handle_text_message(channel_id,
								 pipo_id,
								 transport,
								 username,
								 avatar_url,
								 thread,
								 message,
								 attachments,
								 is_edit)
					.await {
					    eprintln!("Error handling \
						       Message::Text: \
						       {}", e);
					}
				    }
				},
			    }
			},
			None => break
		    }
		}
	    }
	}
	Err(anyhow!("ups"))
    }
}

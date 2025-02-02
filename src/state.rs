use super::config::*;
use super::errors::FxError;
use super::functions::*;
use super::layout::*;
use super::nums::*;
use super::op::*;
use super::session::*;
use super::term::*;

use chrono::prelude::*;
use crossterm::style::Stylize;
use log::{error, info};
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

pub const BEGINNING_ROW: u16 = 3;
pub const FX_CONFIG_DIR: &str = "felix";
pub const CONFIG_FILE: &str = "config.toml";
pub const TRASH: &str = "trash";
pub const WHEN_EMPTY: &str = "Are you sure to empty the trash directory? (if yes: y)";

#[derive(Debug, Clone)]
pub struct State {
    pub list: Vec<ItemInfo>,
    pub registered: Vec<ItemInfo>,
    pub operations: Operation,
    pub current_dir: PathBuf,
    pub trash_dir: PathBuf,
    pub default: String,
    pub commands: Option<HashMap<String, String>>,
    pub sort_by: SortKey,
    pub layout: Layout,
    pub show_hidden: bool,
    pub filtered: bool,
    pub rust_log: Option<String>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct ItemInfo {
    pub file_type: FileType,
    pub file_name: String,
    pub file_path: std::path::PathBuf,
    pub symlink_dir_path: Option<PathBuf>,
    pub file_size: u64,
    pub file_ext: Option<String>,
    pub modified: Option<String>,
    pub is_hidden: bool,
    pub selected: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileType {
    Directory,
    File,
    Symlink,
}

impl State {
    /// Initialize state app.
    pub fn new() -> Result<Self, FxError> {
        let config = read_config().unwrap_or_else(|_| panic!("Something wrong with config file."));
        let session =
            read_session().unwrap_or_else(|_| panic!("Something wrong with session file."));
        let (column, row) =
            crossterm::terminal::size().unwrap_or_else(|_| panic!("Cannot detect terminal size."));

        // Return error if terminal size may cause panic
        if column < 4 {
            error!("Too small terminal size (less than 4 columns).");
            return Err(FxError::TooSmallWindowSize);
        };
        if row < 4 {
            error!("Too small terminal size. (less than 4 rows)");
            return Err(FxError::TooSmallWindowSize);
        };

        let (time_start, name_max) =
            make_layout(column, config.use_full_width, config.item_name_length);

        let has_chafa = check_chafa();
        let is_kitty = check_kitty_support();

        Ok(State {
            list: Vec::new(),
            registered: Vec::new(),
            operations: Operation {
                pos: 0,
                op_list: Vec::new(),
            },
            current_dir: PathBuf::new(),
            trash_dir: PathBuf::new(),
            default: config
                .default
                .unwrap_or_else(|| env::var("EDITOR").expect("Falling back to env var")),
            commands: to_extension_map(&config.exec),
            sort_by: session.sort_by,
            layout: Layout {
                y: BEGINNING_ROW,
                terminal_row: row,
                terminal_column: column,
                name_max_len: name_max,
                time_start_pos: time_start,
                use_full: config.use_full_width,
                option_name_len: config.item_name_length,
                colors: ConfigColor {
                    dir_fg: config.color.dir_fg,
                    file_fg: config.color.file_fg,
                    symlink_fg: config.color.symlink_fg,
                },
                preview: false,
                preview_start_column: column + 2,
                preview_width: column - 1,
                has_chafa,
                is_kitty,
            },
            show_hidden: session.show_hidden,
            filtered: false,
            rust_log: std::env::var("RUST_LOG").ok(),
        })
    }

    /// Reload the app layout when terminal size changes.
    pub fn refresh(&mut self, column: u16, row: u16, nums: &Num, cursor_pos: u16) {
        let (time_start, name_max) =
            make_layout(column, self.layout.use_full, self.layout.option_name_len);

        self.layout.terminal_row = row;
        self.layout.terminal_column = column;
        self.layout.preview_start_column = column + 2;
        self.layout.preview_width = column - 1;
        self.layout.name_max_len = name_max;
        self.layout.time_start_pos = time_start;

        self.redraw(nums, cursor_pos);
    }

    /// Select an item that the cursor points to.
    pub fn get_item(&self, index: usize) -> Result<&ItemInfo, FxError> {
        self.list.get(index).ok_or(FxError::GetItem)
    }

    /// Select an item that the cursor points to, as mut.
    pub fn get_item_mut(&mut self, index: usize) -> Result<&mut ItemInfo, FxError> {
        self.list.get_mut(index).ok_or(FxError::GetItem)
    }

    /// Open the selected file according to the config.
    pub fn open_file(&self, item: &ItemInfo) -> Result<ExitStatus, FxError> {
        let path = &item.file_path;
        let map = &self.commands;
        let extension = item.file_ext.as_ref();

        let mut default = Command::new(&self.default);

        info!("OPEN: {:?}", path);

        match map {
            None => default.arg(path).status().or(Err(FxError::OpenItem)),
            Some(map) => match extension {
                None => default.arg(path).status().or(Err(FxError::OpenItem)),
                Some(extension) => match map.get(extension) {
                    Some(command) => {
                        let mut ex = Command::new(command);
                        ex.arg(path).status().or(Err(FxError::OpenItem))
                    }
                    None => default.arg(path).status().or(Err(FxError::OpenItem)),
                },
            },
        }
    }

    /// Open the selected file in a new window, according to the config.
    pub fn open_file_in_new_window(&self, index: usize) -> Result<Child, FxError> {
        let item = self.get_item(index)?;
        let path = &item.file_path;
        let map = &self.commands;
        let extension = &item.file_ext;

        info!("OPEN(new window): {:?}", path);

        match map {
            None => Err(FxError::OpenNewWindow),
            Some(map) => match extension {
                Some(extension) => match map.get(extension) {
                    Some(command) => {
                        let mut ex = Command::new(command);
                        ex.arg(path)
                            .stdout(Stdio::null())
                            .stdin(Stdio::null())
                            .spawn()
                            .or(Err(FxError::OpenItem))
                    }
                    None => Err(FxError::OpenNewWindow),
                },

                None => Err(FxError::OpenNewWindow),
            },
        }
    }

    /// Move items from the current directory to trash directory.
    /// This does not actually delete items.
    /// If you'd like to delete, use `:empty` after this, or just `:rm`.  
    pub fn remove_and_yank(&mut self, targets: &[ItemInfo], new_op: bool) -> Result<(), FxError> {
        self.registered.clear();
        let total_selected = targets.len();
        let mut trash_vec = Vec::new();
        for (i, item) in targets.iter().enumerate() {
            let item = item.clone();

            print!(" ");
            to_info_bar();
            clear_current_line();
            print!("{}", display_count(i, total_selected));

            match item.file_type {
                FileType::Directory => match self.remove_and_yank_dir(item.clone(), new_op) {
                    Err(e) => {
                        return Err(e);
                    }
                    Ok(path) => trash_vec.push(path),
                },
                FileType::File | FileType::Symlink => {
                    match self.remove_and_yank_file(item.clone(), new_op) {
                        Err(e) => {
                            return Err(e);
                        }
                        Ok(path) => trash_vec.push(path),
                    }
                }
            }
        }
        if new_op {
            self.operations.branch();
            //push deleted item information to operations
            self.operations.push(OpKind::Delete(DeletedFiles {
                trash: trash_vec,
                original: targets.to_vec(),
                dir: self.current_dir.clone(),
            }));
        }

        Ok(())
    }

    /// Move single file to trash directory.
    fn remove_and_yank_file(&mut self, item: ItemInfo, new_op: bool) -> Result<PathBuf, FxError> {
        //prepare from and to for copy
        let from = &item.file_path;
        let mut to = PathBuf::new();

        if item.file_type == FileType::Symlink && !from.exists() {
            match Command::new("rm").arg(from).status() {
                Ok(_) => Ok(PathBuf::new()),
                Err(_) => Err(FxError::RemoveItem(from.to_owned())),
            }
        } else {
            let name = &item.file_name;
            let mut rename = Local::now().timestamp().to_string();
            rename.push('_');
            rename.push_str(name);

            if new_op {
                to = self.trash_dir.join(&rename);

                //copy
                if std::fs::copy(from, &to).is_err() {
                    return Err(FxError::PutItem(from.to_owned()));
                }

                self.push_to_registered(&item, to.clone(), rename);
            }

            //remove original
            if std::fs::remove_file(from).is_err() {
                return Err(FxError::RemoveItem(from.to_owned()));
            }

            Ok(to)
        }
    }

    /// Move single directory recursively to trash directory.
    fn remove_and_yank_dir(&mut self, item: ItemInfo, new_op: bool) -> Result<PathBuf, FxError> {
        let mut trash_name = String::new();
        let mut base: usize = 0;
        let mut trash_path: std::path::PathBuf = PathBuf::new();
        let mut target: PathBuf;

        if new_op {
            let len = walkdir::WalkDir::new(&item.file_path).into_iter().count();
            let unit = len / 5;
            for (i, entry) in walkdir::WalkDir::new(&item.file_path)
                .into_iter()
                .enumerate()
            {
                if i > unit * 4 {
                    print_process("[»»»»-]");
                } else if i > unit * 3 {
                    print_process("[»»»--]");
                } else if i > unit * 2 {
                    print_process("[»»---]");
                } else if i > unit {
                    print_process("[»----]");
                } else if i == 0 {
                    print_process(" [-----]");
                }
                let entry = entry?;
                let entry_path = entry.path();
                if i == 0 {
                    base = entry_path.iter().count();

                    trash_name = chrono::Local::now().timestamp().to_string();
                    trash_name.push('_');
                    let file_name = entry.file_name().to_str();
                    if file_name == None {
                        return Err(FxError::Encode);
                    }
                    trash_name.push_str(file_name.unwrap());
                    trash_path = self.trash_dir.join(&trash_name);
                    std::fs::create_dir(&self.trash_dir.join(&trash_path))?;

                    continue;
                } else {
                    target = entry_path.iter().skip(base).collect();
                    target = trash_path.join(target);
                    if entry.file_type().is_dir() {
                        std::fs::create_dir_all(&target)?;
                        continue;
                    }

                    if let Some(parent) = entry_path.parent() {
                        if !parent.exists() {
                            std::fs::create_dir(parent)?;
                        }
                    }

                    if std::fs::copy(entry_path, &target).is_err() {
                        return Err(FxError::PutItem(entry_path.to_owned()));
                    }
                }
            }

            self.push_to_registered(&item, trash_path.clone(), trash_name);
        }

        //remove original
        if std::fs::remove_dir_all(&item.file_path).is_err() {
            return Err(FxError::RemoveItem(item.file_path));
        }

        Ok(trash_path)
    }

    /// Register removed items to the registry.
    fn push_to_registered(&mut self, item: &ItemInfo, file_path: PathBuf, file_name: String) {
        let mut buf = item.clone();
        buf.file_path = file_path;
        buf.file_name = file_name;
        buf.selected = false;
        self.registered.push(buf);
    }

    /// Register selected items to the registry.
    pub fn yank_item(&mut self, index: usize, selected: bool) {
        self.registered.clear();
        if selected {
            for item in self.list.iter_mut().filter(|item| item.selected) {
                self.registered.push(item.clone());
            }
        } else {
            let item = self.get_item(index).unwrap().clone();
            self.registered.push(item);
        }
    }

    /// Put items in registry to the current directory or target directory.
    /// Only Redo command uses target directory.
    pub fn put_items(
        &mut self,
        targets: &[ItemInfo],
        target_dir: Option<PathBuf>,
    ) -> Result<(), FxError> {
        //make HashSet<String> of file_name
        let mut name_set = HashSet::new();
        match &target_dir {
            None => {
                for item in self.list.iter() {
                    name_set.insert(item.file_name.clone());
                }
            }
            Some(path) => {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    name_set.insert(
                        entry
                            .file_name()
                            .into_string()
                            .unwrap_or_else(|_| "".to_string()),
                    );
                }
            }
        }

        //prepare for operations.push
        let mut put_v = Vec::new();

        let total_selected = targets.len();
        for (i, item) in targets.iter().enumerate() {
            print!(" ");
            to_info_bar();
            clear_current_line();
            print!("{}", display_count(i, total_selected));

            match item.file_type {
                FileType::Directory => {
                    if let Ok(p) = self.put_dir(item, &target_dir, &mut name_set) {
                        put_v.push(p);
                    }
                }
                FileType::File | FileType::Symlink => {
                    if let Ok(q) = self.put_file(item, &target_dir, &mut name_set) {
                        put_v.push(q);
                    }
                }
            }
        }
        if target_dir.is_none() {
            self.operations.branch();
            //push put item information to operations
            self.operations.push(OpKind::Put(PutFiles {
                original: targets.to_owned(),
                put: put_v,
                dir: self.current_dir.clone(),
            }));
        }

        Ok(())
    }

    /// Put single item to current or target directory.
    fn put_file(
        &mut self,
        item: &ItemInfo,
        target_dir: &Option<PathBuf>,
        name_set: &mut HashSet<String>,
    ) -> Result<PathBuf, FxError> {
        match target_dir {
            None => {
                if item.file_path.parent() == Some(&self.trash_dir) {
                    let rename: String = item.file_name.chars().skip(11).collect();
                    let rename = rename_file(&rename, name_set);
                    let to = &self.current_dir.join(&rename);
                    if std::fs::copy(&item.file_path, to).is_err() {
                        return Err(FxError::PutItem(item.file_path.clone()));
                    }
                    name_set.insert(rename);
                    Ok(to.to_path_buf())
                } else {
                    let rename = rename_file(&item.file_name, name_set);
                    let to = &self.current_dir.join(&rename);
                    if std::fs::copy(&item.file_path, to).is_err() {
                        return Err(FxError::PutItem(item.file_path.clone()));
                    }
                    name_set.insert(rename);
                    Ok(to.to_path_buf())
                }
            }
            Some(path) => {
                if item.file_path.parent() == Some(&self.trash_dir) {
                    let rename: String = item.file_name.chars().skip(11).collect();
                    let rename = rename_file(&rename, name_set);
                    let to = path.join(&rename);
                    if std::fs::copy(&item.file_path, to.clone()).is_err() {
                        return Err(FxError::PutItem(item.file_path.clone()));
                    }
                    name_set.insert(rename);
                    Ok(to)
                } else {
                    let rename = rename_file(&item.file_name, name_set);
                    let to = &path.join(&rename);
                    if std::fs::copy(&item.file_path, to).is_err() {
                        return Err(FxError::PutItem(item.file_path.clone()));
                    }
                    name_set.insert(rename);
                    Ok(to.to_path_buf())
                }
            }
        }
    }

    /// Put single directory recursively to current or target directory.
    fn put_dir(
        &mut self,
        buf: &ItemInfo,
        target_dir: &Option<PathBuf>,
        name_set: &mut HashSet<String>,
    ) -> Result<PathBuf, FxError> {
        let mut base: usize = 0;
        let mut target: PathBuf = PathBuf::new();
        let original_path = &buf.file_path;

        let len = walkdir::WalkDir::new(&original_path).into_iter().count();
        let unit = len / 5;
        for (i, entry) in walkdir::WalkDir::new(&original_path)
            .into_iter()
            .enumerate()
        {
            if i > unit * 4 {
                print_process("[»»»»-]");
            } else if i > unit * 3 {
                print_process("[»»»--]");
            } else if i > unit * 2 {
                print_process("[»»---]");
            } else if i > unit {
                print_process("[»----]");
            } else if i == 0 {
                print_process(" [»----]");
            }
            let entry = entry?;
            let entry_path = entry.path();
            if i == 0 {
                base = entry_path.iter().count();

                let parent = &original_path.parent().unwrap();
                if parent == &self.trash_dir {
                    let rename: String = buf.file_name.chars().skip(11).collect();
                    target = match &target_dir {
                        None => self.current_dir.join(&rename),
                        Some(path) => path.join(&rename),
                    };
                    let rename = rename_dir(&rename, name_set);
                    name_set.insert(rename);
                } else {
                    let rename = rename_dir(&buf.file_name, name_set);
                    target = match &target_dir {
                        None => self.current_dir.join(&rename),
                        Some(path) => path.join(&rename),
                    };
                    name_set.insert(rename);
                }
                std::fs::create_dir(&target)?;
                continue;
            } else {
                let child: PathBuf = entry_path.iter().skip(base).collect();
                let child = target.join(child);

                if entry.file_type().is_dir() {
                    std::fs::create_dir_all(child)?;
                    continue;
                } else if let Some(parent) = entry_path.parent() {
                    if !parent.exists() {
                        std::fs::create_dir(parent)?;
                    }
                }

                if std::fs::copy(entry_path, &child).is_err() {
                    return Err(FxError::PutItem(entry_path.to_owned()));
                }
            }
        }
        Ok(target)
    }

    /// Undo operations (put/delete/rename).
    pub fn undo(&mut self, nums: &Num, op: &OpKind) -> Result<(), FxError> {
        match op {
            OpKind::Rename(op) => {
                std::fs::rename(&op.new_name, &op.original_name)?;
                self.operations.pos += 1;
                self.update_list()?;
                self.clear_and_show_headline();
                self.list_up(nums.skip);
                print_info("UNDONE: RENAME", BEGINNING_ROW);
            }
            OpKind::Put(op) => {
                for x in &op.put {
                    if x.is_dir() {
                        std::fs::remove_dir_all(&x)?;
                    } else {
                        std::fs::remove_file(&x)?;
                    }
                }
                self.operations.pos += 1;
                self.update_list()?;
                self.clear_and_show_headline();
                self.list_up(nums.skip);
                print_info("UNDONE: PUT", BEGINNING_ROW);
            }
            OpKind::Delete(op) => {
                let targets = trash_to_info(&self.trash_dir, &op.trash)?;
                self.put_items(&targets, Some(op.dir.clone()))?;
                self.operations.pos += 1;
                self.update_list()?;
                self.clear_and_show_headline();
                self.list_up(nums.skip);
                print_info("UNDONE: DELETE", BEGINNING_ROW);
            }
        }
        relog(op, true);
        Ok(())
    }

    /// Redo operations (put/delete/rename)
    pub fn redo(&mut self, nums: &Num, op: &OpKind) -> Result<(), FxError> {
        match op {
            OpKind::Rename(op) => {
                std::fs::rename(&op.original_name, &op.new_name)?;
                self.operations.pos -= 1;
                self.update_list()?;
                self.clear_and_show_headline();
                self.list_up(nums.skip);
                print_info("REDONE: RENAME", BEGINNING_ROW);
            }
            OpKind::Put(op) => {
                self.put_items(&op.original, Some(op.dir.clone()))?;
                self.operations.pos -= 1;
                self.update_list()?;
                self.clear_and_show_headline();
                self.list_up(nums.skip);
                print_info("REDONE: PUT", BEGINNING_ROW);
            }
            OpKind::Delete(op) => {
                self.remove_and_yank(&op.original, false)?;
                self.operations.pos -= 1;
                self.update_list()?;
                self.clear_and_show_headline();
                self.list_up(nums.skip);
                print_info("REDONE DELETE", BEGINNING_ROW);
            }
        }
        relog(op, false);
        Ok(())
    }

    /// Redraw the contents.
    pub fn redraw(&mut self, nums: &Num, y: u16) {
        self.clear_and_show_headline();
        self.list_up(nums.skip);
        self.move_cursor(nums, y);
    }

    /// Reload the item list and redraw it.
    pub fn reload(&mut self, nums: &Num, y: u16) -> Result<(), FxError> {
        self.update_list()?;
        self.clear_and_show_headline();
        self.list_up(nums.skip);
        self.move_cursor(nums, y);
        Ok(())
    }

    /// Clear all and show the current directory information.
    pub fn clear_and_show_headline(&mut self) {
        clear_all();
        move_to(1, 1);

        //Show current directory path.
        //crossterm's Stylize cannot be applied to PathBuf,
        //current directory does not have any text attribute for now.
        set_color(&TermColor::ForeGround(&Colorname::Cyan));
        print!(" {}", self.current_dir.display(),);
        reset_color();

        //If .git directory exists, get the branch information and print it.
        let git = self.current_dir.join(".git");
        if git.exists() {
            let head = git.join("HEAD");
            if let Ok(head) = std::fs::read(head) {
                let branch: Vec<u8> = head.into_iter().skip(16).collect();
                if let Ok(branch) = std::str::from_utf8(&branch) {
                    print!(" on ",);
                    set_color(&TermColor::ForeGround(&Colorname::LightMagenta));
                    print!("{}", branch.trim().bold());
                    reset_color();
                }
            }
        }

        if self.filtered {
            print!(" (filtered)");
        }
    }

    /// Print an item in the directory.
    fn print(&self, index: usize) {
        let item = &self.get_item(index).unwrap();
        let chars: Vec<char> = item.file_name.chars().collect();
        let name = if chars.len() > self.layout.name_max_len {
            let mut result = chars
                .iter()
                .take(self.layout.name_max_len - 2)
                .collect::<String>();
            result.push_str("..");
            result
        } else {
            item.file_name.clone()
        };
        let time = format_time(&item.modified);
        let selected = &item.selected;
        let color = match item.file_type {
            FileType::Directory => &self.layout.colors.dir_fg,
            FileType::File => &self.layout.colors.file_fg,
            FileType::Symlink => &self.layout.colors.symlink_fg,
        };
        if self.layout.terminal_column < PROPER_WIDTH {
            if *selected {
                set_color(&TermColor::ForeGround(color));
                print!("{}", name.negative(),);
            } else {
                set_color(&TermColor::ForeGround(color));
                print!("{}", name);
                reset_color();
            }
            if self.layout.terminal_column > self.layout.time_start_pos + TIME_WIDTH {
                clear_until_newline();
            }
        } else {
            if *selected {
                set_color(&TermColor::ForeGround(color));
                print!("{}", name.negative(),);
                move_left(100);
                move_right(self.layout.time_start_pos - 1);
                print!(" {}", time.negative());
            } else {
                set_color(&TermColor::ForeGround(color));
                print!("{}", name);
                move_left(100);
                move_right(self.layout.time_start_pos - 1);
                print!(" {}", time);
                reset_color();
            }
            if self.layout.terminal_column > self.layout.time_start_pos + TIME_WIDTH {
                clear_until_newline();
            }
        }
    }

    /// Print items in the directory.
    pub fn list_up(&self, skip_number: u16) {
        let row = self.layout.terminal_row;

        let mut row_count = 0;
        for (i, _) in self.list.iter().enumerate() {
            if i < skip_number as usize {
                continue;
            }
            move_to(3, i as u16 + BEGINNING_ROW - skip_number);
            if row_count == row - BEGINNING_ROW {
                break;
            } else {
                self.print(i);
                row_count += 1;
            }
        }
    }

    /// Update state's list of items.
    pub fn update_list(&mut self) -> Result<(), FxError> {
        let mut result = Vec::new();
        let mut dir_v = Vec::new();
        let mut file_v = Vec::new();

        for entry in fs::read_dir(&self.current_dir)? {
            let e = entry?;
            let entry = make_item(e);
            match entry.file_type {
                FileType::Directory => dir_v.push(entry),
                FileType::File | FileType::Symlink => file_v.push(entry),
            }
        }

        match self.sort_by {
            SortKey::Name => {
                dir_v.sort_by(|a, b| natord::compare(&a.file_name, &b.file_name));
                file_v.sort_by(|a, b| natord::compare(&a.file_name, &b.file_name));
            }
            SortKey::Time => {
                dir_v.sort_by(|a, b| b.modified.partial_cmp(&a.modified).unwrap());
                file_v.sort_by(|a, b| b.modified.partial_cmp(&a.modified).unwrap());
            }
        }

        result.append(&mut dir_v);
        result.append(&mut file_v);

        if !self.show_hidden {
            result.retain(|x| !x.is_hidden);
        }

        self.list = result;
        Ok(())
    }

    /// Reset all item's selected state and exit the select mode.
    pub fn reset_selection(&mut self) {
        for mut item in self.list.iter_mut() {
            item.selected = false;
        }
    }

    /// Select items from the top to current position.
    pub fn select_from_top(&mut self, start_pos: usize) {
        for (i, item) in self.list.iter_mut().enumerate() {
            if i <= start_pos {
                item.selected = true;
            } else {
                item.selected = false;
            }
        }
    }

    /// Select items from the current position to bottom.
    pub fn select_to_bottom(&mut self, start_pos: usize) {
        for (i, item) in self.list.iter_mut().enumerate() {
            if i < start_pos {
                item.selected = false;
            } else {
                item.selected = true;
            }
        }
    }

    /// Change the cursor position, and print item information at the bottom.
    /// If preview is enabled, print text preview, contents of the directory or image preview on the right half of the terminal
    /// (To preview image, you must install chafa. See help).
    pub fn move_cursor(&mut self, nums: &Num, y: u16) {
        if let Ok(item) = self.get_item(nums.index) {
            delete_cursor();

            //Print item information at the bottom
            self.print_footer(nums, item);

            //Print preview if preview is on
            if self.layout.preview {
                self.layout.print_preview(item, y);
            }
        }
        move_to(1, y);
        print_pointer();
        move_left(1);

        //Store cursor position when cursor moves
        self.layout.y = y;
    }

    /// Print item information at the bottom of the terminal.
    fn print_footer(&self, nums: &Num, item: &ItemInfo) {
        move_to(1, self.layout.terminal_row);
        clear_current_line();

        match &item.file_ext {
            Some(ext) => {
                print!(
                    "{}",
                    " ".repeat(self.layout.terminal_column as usize).negative(),
                );
                move_to(1, self.layout.terminal_row);
                let mut footer = format!(
                    "[{}/{}] {} {}",
                    nums.index + 1,
                    self.list.len(),
                    ext.clone(),
                    to_proper_size(item.file_size),
                );
                if self.rust_log.is_some() {
                    let _ = write!(
                        footer,
                        " index:{} skip:{} column:{} row:{}",
                        nums.index,
                        nums.skip,
                        self.layout.terminal_column,
                        self.layout.terminal_row
                    );
                }
                let footer: String = footer
                    .chars()
                    .take(self.layout.terminal_column.into())
                    .collect();
                print!("{}", footer.negative());
            }
            None => {
                print!(
                    "{}",
                    " ".repeat(self.layout.terminal_column as usize).negative(),
                );
                move_to(1, self.layout.terminal_row);
                let mut footer = format!(
                    "[{}/{}] {}",
                    nums.index + 1,
                    self.list.len(),
                    to_proper_size(item.file_size),
                );
                if self.rust_log.is_some() {
                    let _ = write!(
                        footer,
                        " index:{} skip:{} column:{} row:{}",
                        nums.index,
                        nums.skip,
                        self.layout.terminal_column,
                        self.layout.terminal_row
                    );
                }
                let footer: String = footer
                    .chars()
                    .take(self.layout.terminal_column.into())
                    .collect();
                print!("{}", footer.negative());
            }
        }
    }

    /// Store the sort key and whether to show hidden items to session file.
    pub fn write_session(&self, session_path: PathBuf) -> Result<(), FxError> {
        let session = Session {
            sort_by: self.sort_by.clone(),
            show_hidden: self.show_hidden,
        };
        let serialized = toml::to_string(&session)?;
        fs::write(&session_path, serialized)?;
        Ok(())
    }
}

/// Create item information from `std::fs::DirEntry`.
fn make_item(entry: fs::DirEntry) -> ItemInfo {
    let path = entry.path();
    let metadata = fs::symlink_metadata(&path);

    let name = entry
        .file_name()
        .into_string()
        .unwrap_or_else(|_| "Invalid unicode name".to_string());

    let hidden = matches!(name.chars().next(), Some('.'));

    let ext = path.extension().map(|s| {
        s.to_os_string()
            .into_string()
            .unwrap_or_default()
            .to_ascii_lowercase()
    });

    match metadata {
        Ok(metadata) => {
            let time = {
                let sometime = metadata.modified().unwrap();
                let chrono_time: DateTime<Local> = DateTime::from(sometime);
                Some(chrono_time.to_rfc3339_opts(SecondsFormat::Secs, false))
            };

            let filetype = {
                let file_type = metadata.file_type();
                if file_type.is_dir() {
                    FileType::Directory
                } else if file_type.is_file() {
                    FileType::File
                } else if file_type.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::File
                }
            };

            let sym_dir_path = {
                if filetype == FileType::Symlink {
                    if let Ok(sym_meta) = fs::metadata(&path) {
                        if sym_meta.is_dir() {
                            fs::canonicalize(path.clone()).ok()
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            let size = metadata.len();
            ItemInfo {
                file_type: filetype,
                file_name: name,
                file_path: path,
                symlink_dir_path: sym_dir_path,
                file_size: size,
                file_ext: match filetype {
                    FileType::Directory => None,
                    _ => ext,
                },
                modified: time,
                selected: false,
                is_hidden: hidden,
            }
        }
        Err(_) => ItemInfo {
            file_type: FileType::File,
            file_name: name,
            file_path: path,
            symlink_dir_path: None,
            file_size: 0,
            file_ext: ext,
            modified: None,
            selected: false,
            is_hidden: false,
        },
    }
}

/// Generate item information from trash directory, in order to use when redoing.
pub fn trash_to_info(trash_dir: &PathBuf, vec: &[PathBuf]) -> Result<Vec<ItemInfo>, FxError> {
    let total = vec.len();
    let mut count = 0;
    let mut result = Vec::new();
    for entry in fs::read_dir(trash_dir)? {
        let entry = entry?;
        if vec.contains(&entry.path()) {
            result.push(make_item(entry));
            count += 1;
            if count == total {
                break;
            }
        }
    }
    Ok(result)
}

fn check_chafa() -> bool {
    std::process::Command::new("chafa")
        .arg("--help")
        .output()
        .is_ok()
}

// Check if the terminal is Kitty or not
fn check_kitty_support() -> bool {
    if let Ok(term) = std::env::var("TERM") {
        term.contains("kitty")
    } else {
        false
    }
}

#[allow(dead_code)]
fn is_supported_image(item: &ItemInfo) -> bool {
    if let Ok(output) = std::process::Command::new("file")
        .args(["--mime", item.file_path.to_str().unwrap()])
        .output()
    {
        if let Ok(result) = String::from_utf8(output.stdout) {
            let v: Vec<&str> = result.split([':', ';']).collect();
            v[1].contains("image")
        } else {
            false
        }
    } else {
        false
    }
}
